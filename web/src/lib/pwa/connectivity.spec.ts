import { describe, expect, it, vi } from 'vitest';
import { Connectivity } from './connectivity.svelte';

const RECOVERY_MIN_OFFLINE_MS = 2000;
const RECOVERY_INTERVAL_MS = 10_000;

function fakeWindow(onLine: boolean) {
	const document = Object.assign(new EventTarget(), { hidden: false });
	const target = Object.assign(new EventTarget(), { navigator: { onLine }, document });
	return {
		target,
		set(next: boolean) {
			target.navigator.onLine = next;
			target.dispatchEvent(new Event(next ? 'online' : 'offline'));
		},
		hide(hidden: boolean) {
			document.hidden = hidden;
			document.dispatchEvent(new Event('visibilitychange'));
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

	it('reports each return online once, from the browser or a request', async () => {
		vi.useFakeTimers();
		const win = fakeWindow(false);
		const c = new Connectivity();
		c.watch(win.target as unknown as Window);
		const recovered = vi.fn();
		const stop = c.onRecovered(recovered);

		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS);
		win.set(true);
		win.set(true);
		expect(recovered).toHaveBeenCalledTimes(1);

		c.report(false);
		c.report(false);
		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS);
		c.report(true);
		expect(recovered).toHaveBeenCalledTimes(2);

		stop();
		c.report(false);
		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS);
		c.report(true);
		expect(recovered).toHaveBeenCalledTimes(2);
		vi.useRealTimers();
	});

	it('ignores drops too short to have lost anything', async () => {
		vi.useFakeTimers();
		const c = new Connectivity();
		c.watch(fakeWindow(true).target as unknown as Window);
		const recovered = vi.fn();
		c.onRecovered(recovered);

		c.report(false);
		await vi.advanceTimersByTimeAsync(RECOVERY_MIN_OFFLINE_MS - 1);
		c.report(true);
		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS);
		expect(recovered).not.toHaveBeenCalled();

		c.report(false);
		await vi.advanceTimersByTimeAsync(RECOVERY_MIN_OFFLINE_MS);
		c.report(true);
		expect(recovered).toHaveBeenCalledOnce();
		vi.useRealTimers();
	});

	it('recovers at most once per interval on a flaky network, and then once more', async () => {
		vi.useFakeTimers();
		const c = new Connectivity();
		c.watch(fakeWindow(true).target as unknown as Window);
		const recovered = vi.fn();
		c.onRecovered(recovered);
		const flap = async () => {
			c.report(false);
			await vi.advanceTimersByTimeAsync(RECOVERY_MIN_OFFLINE_MS);
			c.report(true);
		};

		await flap();
		await flap();
		await flap();
		expect(recovered).toHaveBeenCalledOnce();
		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS);
		expect(recovered).toHaveBeenCalledTimes(2);
		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS);
		expect(recovered).toHaveBeenCalledTimes(2);
		vi.useRealTimers();
	});

	it('does not ask the server while the app is hidden', async () => {
		vi.useFakeTimers();
		const win = fakeWindow(true);
		const c = new Connectivity();
		const probe = vi.fn(() => Promise.reject(new TypeError('down')));
		const stop = c.watch(win.target as unknown as Window, probe);

		win.hide(true);
		c.report(false);
		await vi.advanceTimersByTimeAsync(60_000);
		expect(probe).not.toHaveBeenCalled();

		win.hide(false);
		await vi.advanceTimersByTimeAsync(1000);
		expect(probe).toHaveBeenCalledOnce();
		win.hide(true);
		await vi.advanceTimersByTimeAsync(60_000);
		expect(probe).toHaveBeenCalledOnce();

		stop();
		vi.useRealTimers();
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
