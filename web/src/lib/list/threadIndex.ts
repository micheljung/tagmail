/**
 * The ordered thread ids of one view (newest first). Lives outside reactive
 * state: 100k numbers in a `$state` proxy would cost more than the list.
 */
export class ThreadIndex {
	#ids: Float64Array;
	#positions: Map<number, number> | null = null;

	constructor(ids: Float64Array = new Float64Array(0)) {
		this.#ids = ids;
	}

	get length(): number {
		return this.#ids.length;
	}

	at(index: number): number | undefined {
		return index >= 0 && index < this.#ids.length ? this.#ids[index] : undefined;
	}

	slice(start: number, end: number): number[] {
		return Array.from(this.#ids.subarray(Math.max(0, start), Math.max(0, end)));
	}

	indexOf(id: number): number {
		if (!this.#positions) {
			this.#positions = new Map();
			for (let i = 0; i < this.#ids.length; i++) this.#positions.set(this.#ids[i], i);
		}
		return this.#positions.get(id) ?? -1;
	}

	has(id: number): boolean {
		return this.indexOf(id) !== -1;
	}

	remove(id: number): boolean {
		const at = this.indexOf(id);
		if (at === -1) return false;
		const next = new Float64Array(this.#ids.length - 1);
		next.set(this.#ids.subarray(0, at));
		next.set(this.#ids.subarray(at + 1), at);
		this.#ids = next;
		this.#positions = null;
		return true;
	}

	/** Drops every id in `ids` in one pass. */
	removeMany(ids: ReadonlySet<number>): void {
		const next = this.#ids.filter((id) => !ids.has(id));
		if (next.length === this.#ids.length) return;
		this.#ids = next;
		this.#positions = null;
	}

	insert(at: number, id: number): void {
		const next = new Float64Array(this.#ids.length + 1);
		next.set(this.#ids.subarray(0, at));
		next[at] = id;
		next.set(this.#ids.subarray(at), at + 1);
		this.#ids = next;
		this.#positions = null;
	}
}

/**
 * Where a thread with `lastDate` belongs in a newest-first list: before the
 * first row whose date is not newer. Only rows inside `known` ranges (all
 * dates loaded) are consulted; `null` means the spot lies in a gap and the
 * caller must refetch the index.
 */
export function insertionPoint(
	count: number,
	lastDate: number,
	dateAt: (index: number) => number | undefined,
	known: readonly { first: number; last: number }[]
): number | null {
	if (count === 0) return 0;
	for (const { first, last } of known) {
		if (first > last || first < 0 || last >= count) continue;
		const head = dateAt(first);
		const tail = dateAt(last);
		if (head === undefined || tail === undefined) continue;
		if (head <= lastDate) {
			if (first === 0) return 0;
			continue;
		}
		if (tail > lastDate) {
			if (last === count - 1) return count;
			continue;
		}
		let lo = first + 1;
		let hi = last;
		while (lo < hi) {
			const mid = (lo + hi) >>> 1;
			const date = dateAt(mid);
			if (date === undefined) return null;
			if (date <= lastDate) hi = mid;
			else lo = mid + 1;
		}
		return lo;
	}
	return null;
}
