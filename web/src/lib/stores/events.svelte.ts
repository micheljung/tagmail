import type { ApiErrorDto, ApiTransport, ChangesDto } from '@tagmail/api-types';

export interface ChangeListener {
	changes(page: ChangesDto): void;
	/** The cache was rebuilt: reload and resolve with the fresh cursor. */
	reset(error: ApiErrorDto): Promise<string | undefined>;
}

/**
 * One event stream for the whole app, fanned out to the open views. Started
 * from the cursor of the first index the app loads, so nothing between that
 * index and the stream is lost; resumes via `Last-Event-ID` and restarts
 * from a freshly loaded cursor after a reset.
 */
export class EventHub {
	connected = $state(false);

	readonly #transport: ApiTransport;
	readonly #listeners = new Set<ChangeListener>();
	#stop: (() => void) | null = null;

	constructor(transport: ApiTransport) {
		this.#transport = transport;
	}

	get running(): boolean {
		return this.#stop !== null;
	}

	subscribe(listener: ChangeListener): () => void {
		this.#listeners.add(listener);
		return () => this.#listeners.delete(listener);
	}

	start(since: string | undefined): void {
		if (this.#stop) return;
		this.connected = true;
		this.#stop = this.#transport.events(since, (event) => {
			if (event.type === 'changes') {
				for (const l of this.#listeners) l.changes(event.data);
				return;
			}
			this.#stop = null;
			this.connected = false;
			void this.#reset(event.data);
		});
	}

	stop(): void {
		this.#stop?.();
		this.#stop = null;
		this.connected = false;
	}

	async #reset(error: ApiErrorDto): Promise<void> {
		const cursors = await Promise.all([...this.#listeners].map((l) => l.reset(error)));
		const fresh = cursors.find((c) => c !== undefined);
		this.start(fresh);
	}
}
