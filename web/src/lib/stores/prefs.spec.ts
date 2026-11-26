import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

function fakeStorage(initial: Record<string, string> = {}) {
	const data = new Map(Object.entries(initial));
	return {
		data,
		getItem: (k: string) => data.get(k) ?? null,
		setItem: (k: string, v: string) => void data.set(k, v),
		removeItem: (k: string) => void data.delete(k)
	};
}

async function load(initial: Record<string, string> = {}, phone = false) {
	vi.resetModules();
	const storage = fakeStorage(initial);
	const root = { dataset: {} as Record<string, string> };
	vi.stubGlobal('localStorage', storage);
	vi.stubGlobal('document', { documentElement: root });
	vi.stubGlobal('matchMedia', (q: string) => ({ matches: phone, media: q }));
	const { prefs } = await import('./prefs.svelte');
	return { prefs, storage, root };
}

describe('prefs store', () => {
	beforeEach(() => vi.resetModules());
	afterEach(() => vi.unstubAllGlobals());

	it('starts from defaults and sets data-density', async () => {
		const { prefs, root } = await load();
		expect(prefs.density).toBe('default');
		expect(prefs.inboxType).toBe('default');
		expect(prefs.readingPane).toBe('none');
		expect(prefs.navWidth).toBe(236);
		expect(prefs.split).toBe(false);
		expect(root.dataset.density).toBe('default');
	});

	it('reads stored values and falls back on invalid ones', async () => {
		const { prefs, root } = await load({
			'tagmail.density': 'compact',
			'tagmail.inboxType': 'bogus',
			'tagmail.readingPane': 'right',
			'tagmail.navWidth': '999'
		});
		expect(prefs.density).toBe('compact');
		expect(prefs.inboxType).toBe('default');
		expect(prefs.readingPane).toBe('right');
		expect(prefs.navWidth).toBe(400);
		expect(prefs.split).toBe(true);
		expect(root.dataset.density).toBe('compact');
	});

	it('is never split on a phone', async () => {
		const { prefs } = await load({ 'tagmail.readingPane': 'below' }, true);
		expect(prefs.phone).toBe(true);
		expect(prefs.split).toBe(false);
	});

	it('persists choices and removes the key for the default', async () => {
		const { prefs, storage, root } = await load();
		prefs.setDensity('comfortable');
		prefs.setInboxType('unread');
		prefs.setReadingPane('below');
		prefs.setNavWidth(300);
		expect(Object.fromEntries(storage.data)).toEqual({
			'tagmail.density': 'comfortable',
			'tagmail.inboxType': 'unread',
			'tagmail.readingPane': 'below',
			'tagmail.navWidth': '300'
		});
		expect(root.dataset.density).toBe('comfortable');
		prefs.setDensity('default');
		prefs.setInboxType('default');
		prefs.setReadingPane('none');
		prefs.setNavWidth(100);
		expect(prefs.navWidth).toBe(200);
		prefs.setNavWidth(236);
		expect([...storage.data.keys()]).toEqual([]);
		expect(root.dataset.density).toBe('default');
	});

	it('works with no browser globals', async () => {
		vi.resetModules();
		const { prefs } = await import('./prefs.svelte');
		prefs.setDensity('compact');
		expect(prefs.density).toBe('compact');
		expect(prefs.phone).toBe(false);
	});
});
