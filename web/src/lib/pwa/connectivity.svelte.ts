/**
 * Whether the server is reachable, from `navigator.onLine` plus the outcome
 * of the last API request: a captive portal or a dead server keeps
 * `onLine` true while every fetch fails.
 */
export class Connectivity {
	#online = $state(true);
	#reachable = $state(true);

	get offline(): boolean {
		return !this.#online || !this.#reachable;
	}

	/** Follows the browser's online/offline events; returns the teardown. */
	watch(target: Window = window): () => void {
		const update = () => {
			this.#online = target.navigator.onLine;
			if (this.#online) this.#reachable = true;
		};
		update();
		target.addEventListener('online', update);
		target.addEventListener('offline', update);
		return () => {
			target.removeEventListener('online', update);
			target.removeEventListener('offline', update);
		};
	}

	report(reachable: boolean): void {
		this.#reachable = reachable;
	}
}

export const connectivity = new Connectivity();
