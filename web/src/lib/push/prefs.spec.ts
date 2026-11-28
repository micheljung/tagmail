import type { PushSubscribeDto, PushSubscriptionDto } from '@tagmail/api-types';
import { IDBFactory } from 'fake-indexeddb';
import { describe, expect, it, vi } from 'vitest';
import { base64url, renewPush, syncPush, type PushApi } from './client';
import { idbPushPrefs, remindersByDefault, type PushPrefs } from './prefs';

const PREFS: PushPrefs = { endpoint: 'https://push.example/a', preview: false, accountIds: [2] };

describe('idbPushPrefs', () => {
	it('has nothing saved at first', async () => {
		const factory = new IDBFactory();
		expect(await idbPushPrefs(() => factory).get()).toBeUndefined();
	});

	it('keeps the settings across store instances, as page and worker share them', async () => {
		const factory = new IDBFactory();
		await idbPushPrefs(() => factory).set(PREFS);
		expect(await idbPushPrefs(() => factory).get()).toEqual(PREFS);
	});

	it('replaces and clears the saved settings', async () => {
		const factory = new IDBFactory();
		const prefs = idbPushPrefs(() => factory);
		await prefs.set(PREFS);
		await prefs.set({ ...PREFS, accountIds: null });
		expect(await prefs.get()).toEqual({ ...PREFS, accountIds: null });
		await prefs.clear();
		expect(await prefs.get()).toBeUndefined();
	});

	it('reads nothing and drops writes when storage fails, instead of failing', async () => {
		const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
		const prefs = idbPushPrefs(() => {
			throw new DOMException('denied', 'SecurityError');
		});
		await expect(prefs.get()).resolves.toBeUndefined();
		await expect(prefs.set(PREFS)).resolves.toBeUndefined();
		await expect(prefs.clear()).resolves.toBeUndefined();
		expect(warn).toHaveBeenCalledTimes(3);
		warn.mockRestore();
	});
});

describe('reminders', () => {
	const KEY = new Uint8Array([4, 1, 2, 3]);
	const OLD: PushPrefs = { endpoint: 'https://push.example/a', preview: true, accountIds: null };
	const subscription = (endpoint: string) => ({
		endpoint,
		options: { applicationServerKey: KEY.slice().buffer },
		toJSON: () => ({ endpoint, keys: { p256dh: 'p', auth: 'a' } }),
		unsubscribe: () => Promise.resolve(true)
	});
	const manager = (endpoint: string) =>
		({
			getSubscription: () => Promise.resolve(subscription(endpoint)),
			subscribe: () => Promise.resolve(subscription('https://push.example/renewed'))
		}) as unknown as PushManager;

	function server() {
		const row: PushSubscriptionDto = {
			id: 1,
			endpointHost: 'push.example',
			preview: true,
			accountIds: null,
			userAgent: null,
			createdAt: 0,
			lastSuccessAt: null,
			current: true,
			reminders: false
		};
		const calls: (PushSubscribeDto | { patch: object })[] = [];
		const api = {
			accounts: () => Promise.resolve([]),
			pushKey: () => Promise.resolve({ publicKey: base64url(KEY) }),
			subscribePush: (dto: PushSubscribeDto) => {
				calls.push(dto);
				row.reminders = dto.reminders ?? row.reminders;
				return Promise.resolve({ ...row });
			},
			updatePushSubscription: (_id: number, patch: { reminders?: boolean }) => {
				calls.push({ patch });
				row.reminders = patch.reminders ?? row.reminders;
				return Promise.resolve({ ...row });
			}
		} as unknown as PushApi;
		return { api, calls, row };
	}

	function memory(value?: PushPrefs) {
		const store = {
			value,
			get: () => Promise.resolve(store.value),
			set: (prefs: PushPrefs) => Promise.resolve(void (store.value = prefs)),
			clear: () => Promise.resolve(void (store.value = undefined))
		};
		return store;
	}

	const android = 'Mozilla/5.0 (Linux; Android 15; Pixel 9) Chrome/147.0 Mobile Safari/537.36';
	const iphone = 'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) Safari/604.1';
	const linux = 'Mozilla/5.0 (X11; Linux x86_64) Chrome/147.0';

	it('default on on desktop and in mobile browser tabs, off in an installed phone app', () => {
		expect(remindersByDefault(linux, false)).toBe(true);
		expect(remindersByDefault(linux, true)).toBe(true);
		expect(remindersByDefault(android, false)).toBe(true);
		expect(remindersByDefault(android, true)).toBe(false);
		expect(remindersByDefault(iphone, true)).toBe(false);
		const ipad = 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Version/18.0 Safari/605.1.15';
		expect(remindersByDefault(ipad, true, 5)).toBe(false);
		expect(remindersByDefault(ipad, true, 0)).toBe(true);
		expect(remindersByDefault(ipad, false, 5)).toBe(true);
	});

	it('are re-sent on every registration and renewal', async () => {
		const { api, calls } = server();
		const prefs = memory({ ...OLD, reminders: false });
		await syncPush(manager(OLD.endpoint), api, prefs, true);
		await renewPush(manager(OLD.endpoint), api, prefs, null);
		expect(calls).toEqual([
			expect.objectContaining({ endpoint: OLD.endpoint, reminders: false }),
			expect.objectContaining({
				endpoint: 'https://push.example/renewed',
				oldEndpoint: OLD.endpoint,
				reminders: false
			})
		]);
		expect(prefs.value).toMatchObject({ reminders: false });
	});

	it('are PATCHed once when the saved settings predate them', async () => {
		const { api, calls, row } = server();
		const prefs = memory(OLD);
		await syncPush(manager(OLD.endpoint), api, prefs, true);
		expect(calls).toHaveLength(2);
		expect(calls[0]).not.toHaveProperty('reminders');
		expect(calls[1]).toEqual({ patch: { reminders: true } });
		expect(row.reminders).toBe(true);
		expect(prefs.value).toEqual({ ...OLD, reminders: true });

		await syncPush(manager(OLD.endpoint), api, prefs, false);
		expect(calls).toHaveLength(3);
		expect(calls[2]).toMatchObject({ reminders: true });
	});
});
