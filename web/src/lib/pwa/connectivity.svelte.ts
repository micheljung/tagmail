const PROBE_FIRST_MS = 1000;
const PROBE_MAX_MS = 30_000;
/** A browser offline blip shorter than this is not worth reloading everything. */
const RECOVERY_MIN_OFFLINE_MS = 2000;
/** On a flaky network, reloading after each blip would itself keep the link saturated. */
const RECOVERY_INTERVAL_MS = 10_000;

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
	#hidden: () => boolean = () => false;
	#offlineSince: number | null = null;
	#requestFailed = false;
	#recoveryPending = false;
	#lastRecovery = -Infinity;
	#recoveryTimer: ReturnType<typeof setTimeout> | undefined;

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
		const doc = target.document;
		this.#hidden = () => doc.hidden;
		const update = () =>
			this.#settle(() => {
				this.#online = target.navigator.onLine;
				if (this.#online) this.#reachable = true;
			});
		const visibility = () => this.#schedule();
		// A request may already have failed; only an "online" event says that changed.
		this.#settle(() => (this.#online = target.navigator.onLine));
		target.addEventListener('online', update);
		target.addEventListener('offline', update);
		doc.addEventListener('visibilitychange', visibility);
		return () => {
			target.removeEventListener('online', update);
			target.removeEventListener('offline', update);
			doc.removeEventListener('visibilitychange', visibility);
			this.#probe = null;
			this.#hidden = () => false;
			clearTimeout(this.#recoveryTimer);
			this.#recoveryTimer = undefined;
			this.#schedule();
		};
	}

	report(reachable: boolean): void {
		this.#settle(() => (this.#reachable = reachable));
		if (!reachable) this.#requestFailed = true;
	}

	/**
	 * Runs `callback` when the app comes back after being offline for a
	 * while, at most once per interval; returns the teardown.
	 */
	onRecovered(callback: () => void): () => void {
		this.#recovered.push(callback);
		return () => {
			this.#recovered = this.#recovered.filter((c) => c !== callback);
		};
	}

	#settle(change: () => void): void {
		const was = this.offline;
		change();
		if (!was && this.offline) this.#offlineSince = Date.now();
		if (was && !this.offline) this.#recover();
		this.#schedule();
	}

	#recover(): void {
		const since = this.#offlineSince;
		const requestFailed = this.#requestFailed;
		this.#offlineSince = null;
		this.#requestFailed = false;
		// A failed request may have lost data however short the outage was.
		if (requestFailed || (since !== null && Date.now() - since >= RECOVERY_MIN_OFFLINE_MS)) {
			this.#recoveryPending = true;
		}
		this.#flushRecovery();
	}

	#flushRecovery(): void {
		if (!this.#recoveryPending || this.offline) return;
		const wait = this.#lastRecovery + RECOVERY_INTERVAL_MS - Date.now();
		if (wait > 0) {
			this.#recoveryTimer ??= setTimeout(() => {
				this.#recoveryTimer = undefined;
				this.#flushRecovery();
			}, wait);
			return;
		}
		this.#recoveryPending = false;
		this.#lastRecovery = Date.now();
		for (const callback of this.#recovered) callback();
	}

	#schedule(): void {
		const probe = this.#probe;
		if (!probe || !this.#online || this.#reachable || this.#hidden()) {
			clearTimeout(this.#probeTimer);
			this.#probeTimer = undefined;
			this.#probeDelay = PROBE_FIRST_MS;
			return;
		}
		if (this.#probeTimer !== undefined) return;
		const timer = setTimeout(() => {
			this.#probeDelay = Math.min(this.#probeDelay * 2, PROBE_MAX_MS);
			const settled = (reachable: boolean) => {
				if (this.#probeTimer === timer) this.#probeTimer = undefined;
				this.report(reachable);
			};
			void probe().then(
				() => settled(true),
				() => settled(false)
			);
		}, this.#probeDelay);
		this.#probeTimer = timer;
	}
}

export const connectivity = new Connectivity();
