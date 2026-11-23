import type { ThreadRowDto } from '@tagmail/api-types';

export const PAGE_SIZE = 100;
export const DEFAULT_CAPACITY = 6000;

export type FetchRows = (ids: number[], signal: AbortSignal) => Promise<ThreadRowDto[]>;

interface Inflight {
	ids: number[];
	controller: AbortController;
}

/**
 * Thread rows keyed by id (so index patches never orphan them), LRU-bounded,
 * fetched in pages of up to 100 ids with in-flight dedupe. Requests whose
 * rows all left the wanted window are aborted.
 */
export class RowCache {
	/** Bumped when rows arrive or are dropped; read it to subscribe. */
	version = $state(0);

	readonly #rows = new Map<number, ThreadRowDto>();
	readonly #pending = new Map<number, Inflight>();
	/** Rows still shown while their refetch runs, so a change never flashes a skeleton. */
	readonly #stale = new Set<number>();
	readonly #fetch: FetchRows;
	readonly #capacity: number;
	readonly #onMissing: (ids: number[]) => void;
	readonly #onError: (error: unknown) => void;

	constructor(
		fetch: FetchRows,
		options: {
			capacity?: number;
			onMissing?: (ids: number[]) => void;
			onError?: (error: unknown) => void;
		} = {}
	) {
		this.#fetch = fetch;
		this.#capacity = options.capacity ?? DEFAULT_CAPACITY;
		this.#onMissing = options.onMissing ?? (() => {});
		this.#onError = options.onError ?? (() => {});
	}

	get size(): number {
		return this.#rows.size;
	}

	/** Reactive read. */
	get(id: number): ThreadRowDto | undefined {
		void this.version;
		return this.#rows.get(id);
	}

	/** Non-reactive read, for patch math. */
	peek(id: number): ThreadRowDto | undefined {
		return this.#rows.get(id);
	}

	isPending(id: number): boolean {
		return this.#pending.has(id);
	}

	/** Loads every id in `wanted` that is neither cached nor in flight. */
	ensure(wanted: readonly number[]): void {
		const keep = new Set(wanted);
		for (const inflight of new Set(this.#pending.values())) {
			if (!inflight.ids.some((id) => keep.has(id))) this.#abort(inflight);
		}
		const missing: number[] = [];
		for (const id of wanted) {
			const row = this.#rows.get(id);
			if (row) {
				this.#rows.delete(id);
				this.#rows.set(id, row);
			}
			if ((!row || this.#stale.has(id)) && !this.#pending.has(id)) missing.push(id);
		}
		for (let i = 0; i < missing.length; i += PAGE_SIZE) {
			this.#load(missing.slice(i, i + PAGE_SIZE));
		}
	}

	/** Rewrites a cached row in place (a confirmed change the server will echo later). */
	update(id: number, patch: (row: ThreadRowDto) => ThreadRowDto): void {
		const row = this.#rows.get(id);
		if (!row) return;
		const next = patch(row);
		if (next === row) return;
		this.#rows.set(id, next);
		this.version++;
	}

	/** Marks a row out of date: it stays visible and the next `ensure` refetches it. */
	invalidate(id: number): void {
		const pending = this.#pending.get(id);
		if (pending) this.#abort(pending);
		if (this.#rows.has(id)) this.#stale.add(id);
	}

	isStale(id: number): boolean {
		return this.#stale.has(id);
	}

	clear(): void {
		for (const inflight of new Set(this.#pending.values())) this.#abort(inflight);
		this.#rows.clear();
		this.#stale.clear();
		this.version++;
	}

	#abort(inflight: Inflight): void {
		inflight.controller.abort();
		for (const id of inflight.ids) {
			if (this.#pending.get(id) === inflight) this.#pending.delete(id);
		}
	}

	#load(ids: number[]): void {
		const inflight: Inflight = { ids, controller: new AbortController() };
		for (const id of ids) this.#pending.set(id, inflight);
		this.#fetch(ids, inflight.controller.signal).then(
			(rows) => {
				if (inflight.controller.signal.aborted) return;
				this.#settle(inflight);
				for (const row of rows) this.#rows.set(row.id, row);
				const got = new Set(rows.map((r) => r.id));
				for (const id of ids) {
					this.#stale.delete(id);
					if (!got.has(id)) this.#rows.delete(id);
				}
				this.#evict();
				this.version++;
				const absent = ids.filter((id) => !got.has(id));
				if (absent.length > 0) this.#onMissing(absent);
			},
			(error: unknown) => {
				if (inflight.controller.signal.aborted) return;
				this.#settle(inflight);
				this.#onError(error);
			}
		);
	}

	#settle(inflight: Inflight): void {
		for (const id of inflight.ids) {
			if (this.#pending.get(id) === inflight) this.#pending.delete(id);
		}
	}

	#evict(): void {
		const excess = this.#rows.size - this.#capacity;
		if (excess <= 0) return;
		const it = this.#rows.keys();
		for (let i = 0; i < excess; i++) {
			const id = it.next().value as number;
			this.#rows.delete(id);
			this.#stale.delete(id);
		}
	}
}
