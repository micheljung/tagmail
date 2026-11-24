const PROBE_FIRST_MS = 1000;
const PROBE_MAX_MS = 30_000;

/**
 * Whether the server is reachable, from `navigator.onLine` plus the outcome
 * of the last API request: a captive portal or a dead server keeps
 * `onLine` true while every fetch fails.
 */
export class Connectivity {
	#online = $state(true);
	#reachable = $state(true);
	#recovered: (() => void)[] = [];
	#probe: (() => Promise<unknown>) | null = null;
	#probeTimer: ReturnType<typeof setTimeout> | undefined;
	#probeDelay = PROBE_FIRST_MS;

	get offline(): boolean {
		return !this.#online || !this.#reachable;
	}

	/**
	 * Follows the browser's online/offline events; returns the teardown.
	 * `probe` asks the server again, with backoff, while the browser claims to
	 * be online but requests fail: "online" often fires before the network
	 * carries traffic, and nothing else would clear the state.
	 */
	watch(target: Window = window, probe?: () => Promise<unknown>): () => void {
		this.#probe = probe ?? null;
		const update = () =>
			this.#settle(() => {
				this.#online = target.navigator.onLine;
				if (this.#online) this.#reachable = true;
			});
		update();
		target.addEventListener('online', update);
		target.addEventListener('offline', update);
		return () => {
			target.removeEventListener('online', update);
			target.removeEventListener('offline', update);
			this.#probe = null;
			this.#schedule();
		};
	}

	report(reachable: boolean): void {
		this.#settle(() => (this.#reachable = reachable));
	}

	/** Runs `callback` each time the app comes back online; returns the teardown. */
	onRecovered(callback: () => void): () => void {
		this.#recovered.push(callback);
		return () => {
			this.#recovered = this.#recovered.filter((c) => c !== callback);
		};
	}

	#settle(change: () => void): void {
		const was = this.offline;
		change();
		if (was && !this.offline) for (const callback of this.#recovered) callback();
		this.#schedule();
	}

	#schedule(): void {
		const probe = this.#probe;
		if (!probe || !this.#online || this.#reachable) {
			clearTimeout(this.#probeTimer);
			this.#probeTimer = undefined;
			this.#probeDelay = PROBE_FIRST_MS;
			return;
		}
		if (this.#probeTimer !== undefined) return;
		this.#probeTimer = setTimeout(() => {
			this.#probeDelay = Math.min(this.#probeDelay * 2, PROBE_MAX_MS);
			void probe().then(
				() => {
					this.#probeTimer = undefined;
					this.report(true);
				},
				() => {
					this.#probeTimer = undefined;
					this.report(false);
				}
			);
		}, this.#probeDelay);
	}
}

export const connectivity = new Connectivity();
