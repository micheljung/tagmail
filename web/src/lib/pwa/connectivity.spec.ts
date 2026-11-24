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

	it('keeps a request failure from before it started watching', () => {
		const c = new Connectivity();
		const recovered = vi.fn();
		c.onRecovered(recovered);
		c.report(false);
		c.watch(fakeWindow(true).target as unknown as Window);
		expect(c.offline).toBe(true);
		expect(recovered).not.toHaveBeenCalled();
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

	it('ignores browser drops too short to have lost anything', async () => {
		vi.useFakeTimers();
		const win = fakeWindow(true);
		const c = new Connectivity();
		c.watch(win.target as unknown as Window);
		const recovered = vi.fn();
		c.onRecovered(recovered);

		win.set(false);
		await vi.advanceTimersByTimeAsync(RECOVERY_MIN_OFFLINE_MS - 1);
		win.set(true);
		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS);
		expect(recovered).not.toHaveBeenCalled();

		win.set(false);
		await vi.advanceTimersByTimeAsync(RECOVERY_MIN_OFFLINE_MS);
		win.set(true);
		expect(recovered).toHaveBeenCalledOnce();
		vi.useRealTimers();
	});

	it('recovers after a short outage that a failed request reported', async () => {
		vi.useFakeTimers();
		const c = new Connectivity();
		c.watch(fakeWindow(true).target as unknown as Window);
		const recovered = vi.fn();
		c.onRecovered(recovered);

		c.report(false);
		await vi.advanceTimersByTimeAsync(RECOVERY_MIN_OFFLINE_MS / 2);
		c.report(true);
		expect(recovered).toHaveBeenCalledOnce();
		vi.useRealTimers();
	});

	it('keeps a throttled recovery until the app is back online', async () => {
		vi.useFakeTimers();
		const win = fakeWindow(true);
		const c = new Connectivity();
		c.watch(win.target as unknown as Window);
		const recovered = vi.fn();
		c.onRecovered(recovered);
		const outage = async (ms: number) => {
			win.set(false);
			await vi.advanceTimersByTimeAsync(ms);
			win.set(true);
		};

		await outage(RECOVERY_MIN_OFFLINE_MS);
		expect(recovered).toHaveBeenCalledOnce();
		await outage(RECOVERY_MIN_OFFLINE_MS);
		const blip = RECOVERY_MIN_OFFLINE_MS / 2;
		await vi.advanceTimersByTimeAsync(RECOVERY_INTERVAL_MS - RECOVERY_MIN_OFFLINE_MS - blip / 2);
		await outage(blip);
		expect(recovered).toHaveBeenCalledTimes(2);
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
