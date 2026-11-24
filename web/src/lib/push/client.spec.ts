import type { PushSubscribeDto, PushSubscriptionDto } from '@tagmail/api-types';
import { describe, expect, it, vi } from 'vitest';
import {
	base64url,
	disablePush,
	enablePush,
	pushAvailability,
	syncPush,
	unsubscribeQuietly,
	type PushApi,
	type PushEnv
} from './client';

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

function fakeApi(key: Uint8Array, rows: PushSubscriptionDto[] = []) {
	const posted: PushSubscribeDto[] = [];
	const removePushSubscription = vi.fn<(id: number) => Promise<void>>(() => Promise.resolve());
	const api: PushApi & { posted: PushSubscribeDto[]; removed: typeof removePushSubscription } = {
		posted,
		removed: removePushSubscription,
		pushKey: () => Promise.resolve({ publicKey: base64url(key) }),
		pushSubscriptions: () => Promise.resolve(rows),
		subscribePush: (dto) => {
			posted.push(dto);
			return Promise.resolve(row());
		},
		updatePushSubscription: (id) => Promise.resolve(row({ id })),
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
		expect(await syncPush(asManager(new FakeManager(null)), api)).toBeNull();
		expect(api.posted).toEqual([]);
	});

	it('upserts the subscription when the key matches', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const manager = new FakeManager(sub);
		const api = fakeApi(OLD_KEY);
		await syncPush(asManager(manager), api);
		expect(sub.unsubscribe).not.toHaveBeenCalled();
		expect(manager.subscribe).not.toHaveBeenCalled();
		expect(api.posted).toEqual([
			{
				endpoint: 'https://push.example/a',
				keys: { p256dh: 'p-https://push.example/a', auth: 'a' }
			}
		]);
	});

	it('resubscribes with the new key and keeps the settings when the key changed', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const manager = new FakeManager(sub);
		const api = fakeApi(NEW_KEY, [
			row({ current: false, id: 9 }),
			row({ preview: false, accountIds: [2] })
		]);
		await syncPush(asManager(manager), api);
		expect(sub.unsubscribe).toHaveBeenCalledOnce();
		expect(manager.subscribe).toHaveBeenCalledWith({
			userVisibleOnly: true,
			applicationServerKey: base64url(NEW_KEY)
		});
		expect(api.posted).toMatchObject([
			{ endpoint: 'https://push.example/1', preview: false, accountIds: [2] }
		]);
	});
});

describe('enablePush', () => {
	it('subscribes only after permission is granted', async () => {
		const manager = new FakeManager(null);
		const api = fakeApi(NEW_KEY);
		const request = vi.fn(() => Promise.resolve('granted' as const));
		expect(await enablePush(asManager(manager), api, request)).toEqual(row());
		expect(request.mock.invocationCallOrder[0]).toBeLessThan(
			manager.subscribe.mock.invocationCallOrder[0]
		);
		expect(api.posted).toHaveLength(1);
	});

	it.each(['denied', 'default'] as const)('stops when permission is %s', async (answer) => {
		const manager = new FakeManager(null);
		const api = fakeApi(NEW_KEY);
		expect(await enablePush(asManager(manager), api, () => Promise.resolve(answer))).toBeNull();
		expect(manager.subscribe).not.toHaveBeenCalled();
		expect(api.posted).toEqual([]);
	});

	it('replaces a subscription made with another key', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const manager = new FakeManager(sub);
		await enablePush(asManager(manager), fakeApi(NEW_KEY), () => Promise.resolve('granted'));
		expect(sub.unsubscribe).toHaveBeenCalledOnce();
		expect(manager.subscribe).toHaveBeenCalledOnce();
	});
});

describe('disablePush', () => {
	it('unsubscribes the browser and removes the server row', async () => {
		const sub = new FakeSubscription('https://push.example/a', OLD_KEY);
		const api = fakeApi(OLD_KEY);
		await disablePush(asManager(new FakeManager(sub)), api, row({ id: 4 }));
		expect(sub.unsubscribe).toHaveBeenCalledOnce();
		expect(api.removed).toHaveBeenCalledWith(4);
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
