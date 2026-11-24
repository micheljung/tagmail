import { IDBFactory } from 'fake-indexeddb';
import { describe, expect, it, vi } from 'vitest';
import { idbPushPrefs, type PushPrefs } from './prefs';

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
