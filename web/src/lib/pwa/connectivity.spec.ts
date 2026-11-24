import { describe, expect, it, vi } from 'vitest';
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

	it('reports each return online once, from the browser or a request', () => {
		const win = fakeWindow(false);
		const c = new Connectivity();
		c.watch(win.target as unknown as Window);
		const recovered = vi.fn();
		const stop = c.onRecovered(recovered);

		win.set(true);
		win.set(true);
		expect(recovered).toHaveBeenCalledTimes(1);

		c.report(false);
		c.report(false);
		c.report(true);
		expect(recovered).toHaveBeenCalledTimes(2);

		stop();
		c.report(false);
		c.report(true);
		expect(recovered).toHaveBeenCalledTimes(2);
	});

	it('asks the server again with backoff while online but unreachable', async () => {
		vi.useFakeTimers();
		const win = fakeWindow(true);
		const c = new Connectivity();
		let up = false;
		const probe = vi.fn(() => (up ? Promise.resolve() : Promise.reject(new TypeError('down'))));
		const recovered = vi.fn();
		c.onRecovered(recovered);
		const stop = c.watch(win.target as unknown as Window, probe);

		c.report(false);
		await vi.advanceTimersByTimeAsync(1000);
		expect(probe).toHaveBeenCalledTimes(1);
		await vi.advanceTimersByTimeAsync(1999);
		expect(probe).toHaveBeenCalledTimes(1);
		up = true;
		await vi.advanceTimersByTimeAsync(1);
		expect(probe).toHaveBeenCalledTimes(2);
		expect(c.offline).toBe(false);
		expect(recovered).toHaveBeenCalledOnce();

		win.set(false);
		await vi.advanceTimersByTimeAsync(60_000);
		expect(probe).toHaveBeenCalledTimes(2);

		stop();
		vi.useRealTimers();
	});
});
