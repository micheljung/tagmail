import { describe, expect, it } from 'vitest';
import { Connectivity } from './connectivity.svelte';

function fakeWindow(onLine: boolean) {
	const target = new EventTarget() as EventTarget & { navigator: { onLine: boolean } };
	target.navigator = { onLine };
	return {
		target,
		set(next: boolean) {
			target.navigator.onLine = next;
			target.dispatchEvent(new Event(next ? 'online' : 'offline'));
		}
	};
}

describe('Connectivity', () => {
	it('follows the browser and failed requests', () => {
		const win = fakeWindow(true);
		const c = new Connectivity();
		const stop = c.watch(win.target as unknown as Window);
		expect(c.offline).toBe(false);

		c.report(false);
		expect(c.offline).toBe(true);
		c.report(true);
		expect(c.offline).toBe(false);

		win.set(false);
		expect(c.offline).toBe(true);
		c.report(true);
		expect(c.offline).toBe(true);

		c.report(false);
		win.set(true);
		expect(c.offline).toBe(false);

		stop();
		win.set(false);
		expect(c.offline).toBe(false);
	});

	it('starts offline when the browser is', () => {
		const c = new Connectivity();
		c.watch(fakeWindow(false).target as unknown as Window);
		expect(c.offline).toBe(true);
	});
});
