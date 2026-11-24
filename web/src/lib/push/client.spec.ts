import type { AccountDto, PushSubscribeDto, PushSubscriptionDto } from '@tagmail/api-types';
import { describe, expect, it, vi } from 'vitest';
import {
	base64url,
	disablePush,
	enablePush,
	fetchPushApi,
	pushAvailability,
	pushSyncer,
	renewPush,
	syncPush,
	unsubscribeQuietly,
	updatePush,
	type PushApi,
	type PushEnv
} from './client';
import type { PushPrefStore, PushPrefs } from './prefs';

const OLD_KEY = new Uint8Array([4, 1, 2, 3]);
const NEW_KEY = new Uint8Array([4, 9, 8, 7]);

class FakeSubscription {
	readonly unsubscribe = vi.fn(() => Promise.resolve(true));
	readonly options: { applicationServerKey: ArrayBuffer | null };
	constructor(
		readonly endpoint: string,
		key: Uint8Array | null
	) {
		this.options = { applicationServerKey: key ? key.slice().buffer : null };
	}
	toJSON() {
		return { endpoint: this.endpoint, keys: { p256dh: `p-${this.endpoint}`, auth: 'a' } };
	}
}

class FakeManager {
	current: FakeSubscription | null;
	readonly subscribe = vi.fn((options: { applicationServerKey: string }) => {
		const key = Uint8Array.from(
			atob(options.applicationServerKey.replaceAll('-', '+').replaceAll('_', '/')),
			(c) => c.charCodeAt(0)
		);
		this.current = new FakeSubscription(
			`https://push.example/${this.subscribe.mock.calls.length}`,
			key
		);
		return Promise.resolve(this.current);
	});
	constructor(current: FakeSubscription | null) {
		this.current = current;
	}
	getSubscription() {
		return Promise.resolve(this.current);
	}
}

function row(overrides: Partial<PushSubscriptionDto> = {}): PushSubscriptionDto {
	return {
		id: 1,
		endpointHost: 'push.example',
		preview: true,
		accountIds: null,
		userAgent: null,
		createdAt: 0,
		lastSuccessAt: null,
		current: true,
		...overrides
	};
}

function memoryPrefs(initial?: PushPrefs): PushPrefStore & { value: PushPrefs | undefined } {
	const store = {
		value: initial,
		get: () => Promise.resolve(store.value),
		set: (prefs: PushPrefs) => {
			store.value = prefs;
			return Promise.resolve();
		},
		clear: () => {
			store.value = undefined;
			return Promise.resolve();
		}
	};
	return store;
}

const SAVED: PushPrefs = { endpoint: 'https://push.example/a', preview: false, accountIds: [2] };
const refusal = { status: 422, field: 'accountIds' };
const SAVED_KEYS = { p256dh: 'p', auth: 'a' };

function fakeApi(key: Uint8Array, rows: PushSubscriptionDto[] = [], accountIds = [1, 2, 3]) {
	const posted: PushSubscribeDto[] = [];
	const removePushSubscription = vi.fn<(id: number) => Promise<void>>(() => Promise.resolve());
	const api: PushApi & { posted: PushSubscribeDto[]; removed: typeof removePushSubscription } = {
		posted,
		removed: removePushSubscription,
		accounts: () => Promise.resolve(accountIds.map((id) => ({ id }) as AccountDto)),
		pushKey: () => Promise.resolve({ publicKey: base64url(key) }),
		pushSubscriptions: () => Promise.resolve(rows),
		subscribePush: (dto) => {
			posted.push(dto);
			return Promise.resolve(
				row({ preview: dto.preview ?? true, accountIds: dto.accountIds ?? null })
			);
		},
		updatePushSubscription: (id, patch) =>
			Promise.resolve(
				row({ id, preview: patch.preview ?? true, accountIds: patch.accountIds ?? null })
			),
		removePushSubscription
	};
	return api;
}

const asManager = (m: FakeManager) => m as unknown as PushManager;

describe('base64url', () => {
	it('encodes without padding in the URL alphabet', () => {
		expect(base64url(new Uint8Array([0xfb, 0xff, 0xfe]))).toBe('-__-');
		expect(base64url(new Uint8Array([1]).buffer)).toBe('AQ');
	});
});

describe('pushAvailability', () => {
	const env: PushEnv = {
		userAgent: 'Mozilla/5.0 (X11; Linux x86_64) Chrome/147.0',
		standalone: undefined,
		serviceWorker: true,
		pushManager: true,
		notification: true,
		permission: 'default'
	};
	const iphone = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) Safari/604.1';
	it.each([
		['a desktop browser', env, 'ready'],
		['a denied permission', { ...env, permission: 'denied' as const }, 'blocked'],
		['no Push API', { ...env, pushManager: false }, 'unsupported'],
		['no service worker', { ...env, serviceWorker: false }, 'unsupported'],
		['no Notification API', { ...env, notification: false }, 'unsupported'],
		['an iPhone Safari tab', { ...env, userAgent: iphone, pushManager: false }, 'ios-tab'],
		[
			'an iPad tab with a Mac user agent',
			{ ...env, standalone: false, pushManager: false },
			'ios-tab'
		],
		['an iPhone Home Screen app', { ...env, userAgent: iphone, standalone: true }, 'ready']
	])('%s', (_name, input, expected) => {
		expect(pushAvailability(input)).toBe(expected);
	});
});

describe('syncPush', () => {
	it('does nothing without a subscription', async () => {
		const api = fakeApi(OLD_KEY);
		expect(await syncPush(asManager(new FakeManager(null)), api, memoryPrefs())).toBeNull();
		expect(api.posted).toEqual([]);
	});

	it('upserts the subscription when the key matches and remembers the result', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const manager = new FakeManager(sub);
		const api = fakeApi(OLD_KEY);
		const prefs = memoryPrefs();
		await syncPush(asManager(manager), api, prefs);
		expect(sub.unsubscribe).not.toHaveBeenCalled();
		expect(manager.subscribe).not.toHaveBeenCalled();
		expect(api.posted).toEqual([
			{
				endpoint: 'https://push.example/a',
				keys: { p256dh: 'p-https://push.example/a', auth: 'a' }
			}
		]);
		expect(prefs.value).toEqual({
			endpoint: 'https://push.example/a',
			preview: true,
			accountIds: null
		});
	});

	it('sends the saved settings, so a server that lost the row keeps them', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY);
		const mine = await syncPush(asManager(new FakeManager(sub)), api, memoryPrefs(SAVED));
		expect(api.posted).toMatchObject([
			{ endpoint: 'https://push.example/a', preview: false, accountIds: [2] }
		]);
		expect(api.posted[0]).not.toHaveProperty('oldEndpoint');
		expect(mine).toMatchObject({ preview: false, accountIds: [2] });
	});

	it('sends "all accounts" by leaving the accounts out', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY);
		await syncPush(
			asManager(new FakeManager(sub)),
			api,
			memoryPrefs({ ...SAVED, accountIds: null })
		);
		expect(api.posted[0]).not.toHaveProperty('accountIds');
	});

	it('leaves out accounts removed since the settings were saved', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY, [], [1, 3]);
		await syncPush(
			asManager(new FakeManager(sub)),
			api,
			memoryPrefs({ ...SAVED, accountIds: [2, 3] })
		);
		expect(api.posted[0].accountIds).toEqual([3]);
	});

	it('falls back to all accounts when every saved account was removed', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY, [], [1]);
		const prefs = memoryPrefs(SAVED);
		await syncPush(asManager(new FakeManager(sub)), api, prefs);
		expect(api.posted[0]).not.toHaveProperty('accountIds');
		expect(prefs.value).toMatchObject({ preview: false, accountIds: null });
	});

	it('registers without the account filter once when the server refuses it', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY);
		api.accounts = () => Promise.reject(new Error('offline'));
		const accepting = fakeApi(OLD_KEY);
		api.subscribePush = (dto) => {
			api.posted.push(dto);
			return dto.accountIds
				? Promise.reject(Object.assign(new Error('unknown accounts'), refusal))
				: accepting.subscribePush(dto);
		};
		const prefs = memoryPrefs(SAVED);
		const mine = await syncPush(asManager(new FakeManager(sub)), api, prefs);
		expect(api.posted).toMatchObject([{ accountIds: [2] }, { preview: false }]);
		expect(api.posted[1]).not.toHaveProperty('accountIds');
		expect(mine).toMatchObject({ preview: false, accountIds: null });
		expect(prefs.value).toEqual({ ...SAVED, accountIds: null });
	});

	it('does not retry other failures', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY);
		api.subscribePush = (dto) => {
			api.posted.push(dto);
			return Promise.reject(Object.assign(new Error('bad keys'), { ...refusal, field: 'keys' }));
		};
		await expect(
			syncPush(asManager(new FakeManager(sub)), api, memoryPrefs(SAVED))
		).rejects.toThrow('bad keys');
		expect(api.posted).toHaveLength(1);
	});

	it('names the saved endpoint when the browser renewed it unnoticed', async () => {
		const sub = new FakeSubscription('https://push.example/b', OLD_KEY);
		const api = fakeApi(OLD_KEY);
		await syncPush(asManager(new FakeManager(sub)), api, memoryPrefs(SAVED));
		expect(api.posted).toMatchObject([
			{ endpoint: 'https://push.example/b', oldEndpoint: 'https://push.example/a' }
		]);
	});

	it('resubscribes with the new key in place of the old endpoint', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const manager = new FakeManager(sub);
		const api = fakeApi(NEW_KEY);
		const prefs = memoryPrefs(SAVED);
		await syncPush(asManager(manager), api, prefs);
		expect(sub.unsubscribe).toHaveBeenCalledOnce();
		expect(manager.subscribe).toHaveBeenCalledWith({
			userVisibleOnly: true,
			applicationServerKey: base64url(NEW_KEY)
		});
		expect(api.posted).toMatchObject([
			{
				endpoint: 'https://push.example/1',
				oldEndpoint: 'https://push.example/a',
				preview: false,
				accountIds: [2]
			}
		]);
		expect(prefs.value?.endpoint).toBe('https://push.example/1');
	});

	it('replaces the old endpoint on a key change even without saved settings', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(NEW_KEY);
		await syncPush(asManager(new FakeManager(sub)), api, memoryPrefs());
		expect(api.posted).toEqual([
			{
				endpoint: 'https://push.example/1',
				keys: { p256dh: 'p-https://push.example/1', auth: 'a' },
				oldEndpoint: 'https://push.example/a'
			}
		]);
		expect(api.removed).not.toHaveBeenCalled();
	});
});

describe('enablePush', () => {
	it('subscribes only after permission is granted', async () => {
		const manager = new FakeManager(null);
		const api = fakeApi(NEW_KEY);
		const prefs = memoryPrefs();
		const request = vi.fn(() => Promise.resolve('granted' as const));
		expect(await enablePush(asManager(manager), api, prefs, request)).toEqual(row());
		expect(request.mock.invocationCallOrder[0]).toBeLessThan(
			manager.subscribe.mock.invocationCallOrder[0]
		);
		expect(api.posted).toHaveLength(1);
		expect(prefs.value?.endpoint).toBe('https://push.example/1');
	});

	it.each(['denied', 'default'] as const)('stops when permission is %s', async (answer) => {
		const manager = new FakeManager(null);
		const api = fakeApi(NEW_KEY);
		expect(
			await enablePush(asManager(manager), api, memoryPrefs(), () => Promise.resolve(answer))
		).toBeNull();
		expect(manager.subscribe).not.toHaveBeenCalled();
		expect(api.posted).toEqual([]);
	});

	it('replaces a subscription made with another key', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const manager = new FakeManager(sub);
		const api = fakeApi(NEW_KEY);
		await enablePush(asManager(manager), api, memoryPrefs(), () => Promise.resolve('granted'));
		expect(sub.unsubscribe).toHaveBeenCalledOnce();
		expect(manager.subscribe).toHaveBeenCalledOnce();
		expect(api.posted).toMatchObject([{ oldEndpoint: 'https://push.example/a' }]);
	});
});

describe('updatePush', () => {
	it('remembers the applied settings for this endpoint', async () => {
		const prefs = memoryPrefs({ ...SAVED, preview: true });
		await updatePush(fakeApi(OLD_KEY), prefs, 1, { preview: false });
		expect(prefs.value).toEqual({ ...SAVED, accountIds: null });
	});
});

describe('disablePush', () => {
	it('unsubscribes the browser, forgets the settings and removes the server row', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY);
		const prefs = memoryPrefs(SAVED);
		await disablePush(asManager(new FakeManager(sub)), api, prefs, row({ id: 4 }));
		expect(sub.unsubscribe).toHaveBeenCalledOnce();
		expect(prefs.value).toBeUndefined();
		expect(api.removed).toHaveBeenCalledWith(4);
	});
});

describe('renewPush', () => {
	it('registers the renewed endpoint in place of the old one with the saved settings', async () => {
		const old = new FakeSubscription('https://push.example/a', OLD_KEY);
		const manager = new FakeManager(null);
		const api = fakeApi(NEW_KEY);
		const prefs = memoryPrefs(SAVED);
		await renewPush(asManager(manager), api, prefs, old as unknown as PushSubscription);
		expect(manager.subscribe).toHaveBeenCalledWith({
			userVisibleOnly: true,
			applicationServerKey: base64url(OLD_KEY)
		});
		expect(api.posted).toMatchObject([
			{
				endpoint: 'https://push.example/1',
				oldEndpoint: 'https://push.example/a',
				preview: false,
				accountIds: [2]
			}
		]);
		expect(prefs.value).toEqual({ ...SAVED, endpoint: 'https://push.example/1' });
	});

	it('subscribes with the server key when the browser passes no old subscription', async () => {
		const manager = new FakeManager(null);
		const api = fakeApi(NEW_KEY);
		await renewPush(asManager(manager), api, memoryPrefs(SAVED), null);
		expect(manager.subscribe).toHaveBeenCalledWith({
			userVisibleOnly: true,
			applicationServerKey: base64url(NEW_KEY)
		});
		expect(api.posted).toMatchObject([
			{ endpoint: 'https://push.example/1', oldEndpoint: 'https://push.example/a', preview: false }
		]);
	});
});

describe('fetchPushApi', () => {
	it('posts with the CSRF header and the session cookie', async () => {
		const fetcher = vi.fn<typeof fetch>(() =>
			Promise.resolve(new Response(JSON.stringify(row()), { status: 201 }))
		);
		const dto: PushSubscribeDto = {
			endpoint: 'https://push.example/a',
			keys: { p256dh: 'p', auth: 'a' }
		};
		expect(await fetchPushApi(fetcher).subscribePush(dto)).toEqual(row());
		const [path, init] = fetcher.mock.calls[0];
		expect(path).toBe('/api/push/subscriptions');
		expect(init).toMatchObject({ method: 'POST', credentials: 'same-origin' });
		expect(new Headers(init?.headers).get('X-Requested-With')).toBe('tagmail');
		expect(JSON.parse(init?.body as string)).toEqual(dto);
	});

	it('fails on an error status', async () => {
		const fetcher = () => Promise.resolve(new Response('', { status: 401 }));
		await expect(fetchPushApi(fetcher).pushKey()).rejects.toThrow('401');
	});

	it('reports the status and the refused field', async () => {
		const fetcher = () =>
			Promise.resolve(
				new Response(JSON.stringify({ error: 'invalid', message: 'x', field: 'accountIds' }), {
					status: 422
				})
			);
		await expect(
			fetchPushApi(fetcher).subscribePush({ endpoint: 'https://push.example/a', keys: SAVED_KEYS })
		).rejects.toMatchObject(refusal);
	});

	it('lists the accounts', async () => {
		const fetcher = vi.fn<typeof fetch>(() =>
			Promise.resolve(new Response(JSON.stringify({ accounts: [{ id: 4 }] })))
		);
		expect(await fetchPushApi(fetcher).accounts()).toEqual([{ id: 4 }]);
		expect(fetcher.mock.calls[0][0]).toBe('/api/accounts');
	});
});

describe('pushSyncer', () => {
	it('syncs once per session and shares a run in flight', async () => {
		const sync = vi.fn(() => Promise.resolve());
		const syncFor = pushSyncer(sync);
		await Promise.all([syncFor('a'), syncFor('a')]);
		await syncFor('a');
		expect(sync).toHaveBeenCalledOnce();
		await syncFor('b');
		expect(sync).toHaveBeenCalledTimes(2);
	});

	it('tries again after a failed run', async () => {
		vi.spyOn(console, 'warn').mockImplementation(() => {});
		const sync = vi
			.fn()
			.mockRejectedValueOnce(new TypeError('offline'))
			.mockResolvedValue(undefined);
		const syncFor = pushSyncer(sync);
		await syncFor('a');
		await syncFor('a');
		await syncFor('a');
		expect(sync).toHaveBeenCalledTimes(2);
		vi.restoreAllMocks();
	});
});

describe('unsubscribeQuietly', () => {
	it('unsubscribes when the browser answers', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		await unsubscribeQuietly(() => Promise.resolve(asManager(new FakeManager(sub))));
		expect(sub.unsubscribe).toHaveBeenCalledOnce();
	});

	it('swallows failures', async () => {
		await expect(
			unsubscribeQuietly(() => Promise.reject(new Error('gone')))
		).resolves.toBeUndefined();
	});

	it('gives up after the timeout', async () => {
		vi.useFakeTimers();
		const done = unsubscribeQuietly(() => new Promise<PushManager | null>(() => {}), 2000);
		await vi.advanceTimersByTimeAsync(2000);
		await expect(done).resolves.toBeUndefined();
		vi.useRealTimers();
	});
});
