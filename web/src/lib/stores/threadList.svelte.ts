import type { ChangeDto, ChangesDto, ThreadRowDto } from '@tagmail/api-types';
import { SvelteSet } from 'svelte/reactivity';
import { ThreadIndex, insertionPoint } from '../list/threadIndex';
import { PAGE_SIZE, RowCache } from '../list/rowCache.svelte';
import type { Range } from '../list/math';
import type { Api, ViewId } from '../transport';
import { viewScope } from '../views';
import { parseCursor, type Cursor } from './cursor';
import type { ChangeListener, EventHub } from './events.svelte';
import type { NavStore } from './nav.svelte';
import type { OpList, Removed } from '../ops/ops.svelte';

export const REFETCH_DEBOUNCE_MS = 300;

interface ThreadPayload {
	accountId?: number;
	lastDate?: number;
	mailboxes?: number[];
	removedFrom?: number[];
	labels?: string[];
}

interface MessagePayload {
	threadId?: number;
}

export type ListStatus = 'loading' | 'ready' | 'error';

/**
 * One thread list (inbox, a label, a search): its id index, row cache,
 * selection (by thread id, so patches never shift it) and live patching
 * from the change feed.
 */
export class ThreadList implements ChangeListener, OpList {
	readonly view: ViewId;
	readonly rows: RowCache;
	index = new ThreadIndex();

	indexVersion = $state(0);
	count = $state(0);
	status = $state<ListStatus>('loading');
	error = $state<string | null>(null);
	selectedId = $state<number | null>(null);
	/** Threads ticked for a bulk action (`x`, checkboxes). */
	readonly checked = new SvelteSet<number>();
	/** Index of the last ticked row, the start of a shift-click range. */
	checkAnchor = -1;
	/** Virtual scroll offset, kept while the list is off screen. */
	scrollOffset = 0;
	/** Rows currently rendered; patches only trust dates inside it. */
	window: Range = { first: 0, last: -1 };
	/** Whether a list view shows it; detached lists don't fetch rows. */
	attached = false;

	readonly #api: Api;
	readonly #hub: EventHub;
	readonly #nav: NavStore;
	readonly #unsubscribe: () => void;
	#cursor: Cursor | null = null;
	#refetchTimer: ReturnType<typeof setTimeout> | null = null;
	#loadedOnce = false;
	#lastSelectedIndex = 0;
	#direction = 1;

	constructor(view: ViewId, api: Api, hub: EventHub, nav: NavStore) {
		this.view = view;
		this.#api = api;
		this.#hub = hub;
		this.#nav = nav;
		this.rows = new RowCache((ids, signal) => api.threads(ids, signal).then((r) => r.threads), {
			onMissing: () => this.scheduleRefetch(),
			onError: (e) => (this.error = e instanceof Error ? e.message : String(e))
		});
		this.#unsubscribe = hub.subscribe(this);
	}

	activeIndex = $derived.by(() => {
		void this.indexVersion;
		return this.selectedId === null ? -1 : this.index.indexOf(this.selectedId);
	});

	async load(): Promise<string | undefined> {
		if (!this.#loadedOnce) this.status = 'loading';
		try {
			const { ids, cursor } = await this.#api.viewIndex(this.view);
			this.index = new ThreadIndex(ids);
			this.#cursor = parseCursor(cursor);
			this.#loadedOnce = true;
			this.#indexChanged();
			if (this.selectedId === null && this.count > 0) this.select(0);
			this.status = 'ready';
			this.error = null;
			if (!this.#hub.running) this.#hub.start(cursor || undefined);
			if (this.attached) this.ensureWindow(this.window);
			return cursor || undefined;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
			if (!this.#loadedOnce) this.status = 'error';
			return undefined;
		}
	}

	dispose(): void {
		this.#unsubscribe();
		if (this.#refetchTimer) clearTimeout(this.#refetchTimer);
		this.rows.clear();
	}

	idAt(index: number): number | undefined {
		return this.index.at(index);
	}

	select(index: number): void {
		const id = this.index.at(index);
		if (id === undefined) return;
		this.#direction = index >= this.#lastSelectedIndex ? 1 : -1;
		this.selectedId = id;
		this.#lastSelectedIndex = index;
	}

	/** Moves the selection; with nothing selected, selects the first row. */
	move(delta: number): void {
		if (this.count === 0) return;
		const from = this.activeIndex;
		const to = from === -1 ? 0 : Math.min(this.count - 1, Math.max(0, from + delta));
		this.select(to);
	}

	/** Neighbour of thread `id` for j/k inside an open conversation. */
	neighbour(id: number, delta: number): number | undefined {
		const at = this.index.indexOf(id);
		return at === -1 ? undefined : this.index.at(at + delta);
	}

	/** Ticks or unticks the row at `index`; with `range`, everything from the anchor. */
	toggleChecked(index: number, range = false): void {
		const id = this.index.at(index);
		if (id === undefined) return;
		const on = !this.checked.has(id);
		if (range && this.checkAnchor !== -1) {
			const [from, to] = [Math.min(this.checkAnchor, index), Math.max(this.checkAnchor, index)];
			for (const rowId of this.index.slice(from, to + 1)) {
				if (on) this.checked.add(rowId);
				else this.checked.delete(rowId);
			}
		} else if (on) this.checked.add(id);
		else this.checked.delete(id);
		this.checkAnchor = index;
	}

	/** Ticks the rows on screen (Gmail's "select all" is per page), or clears the ticks. */
	setAllChecked(on: boolean): void {
		this.checked.clear();
		this.checkAnchor = -1;
		if (!on) return;
		for (const id of this.index.slice(this.window.first, this.window.last + 1)) {
			this.checked.add(id);
		}
	}

	/** Threads an action applies to: the ticked ones, else the selected one. */
	targets(): number[] {
		if (this.checked.size > 0) {
			return this.index.slice(0, this.count).filter((id) => this.checked.has(id));
		}
		return this.selectedId === null ? [] : [this.selectedId];
	}

	takeOut(ids: readonly number[]): Removed[] {
		const out: Removed[] = [];
		for (const id of ids) {
			const at = this.index.indexOf(id);
			if (at !== -1) out.push({ id, at });
		}
		if (out.length === 0) return out;
		out.sort((a, b) => a.at - b.at);
		const gone = new Set(out.map((r) => r.id));
		const selectedAt = this.selectedId === null ? -1 : this.index.indexOf(this.selectedId);
		this.index.removeMany(gone);
		if (this.selectedId !== null && gone.has(this.selectedId)) {
			const before = out.filter((r) => r.at < selectedAt).length;
			const at = Math.min(selectedAt - before, this.index.length - 1);
			this.selectedId = this.index.at(at) ?? null;
			this.#lastSelectedIndex = Math.max(0, at);
		}
		for (const id of gone) this.checked.delete(id);
		this.#indexChanged();
		if (this.attached) this.ensureWindow(this.window);
		return out;
	}

	putBack(entries: readonly Removed[]): void {
		let changed = false;
		for (const { id, at } of [...entries].sort((a, b) => a.at - b.at)) {
			if (this.index.has(id)) continue;
			this.index.insert(Math.min(at, this.index.length), id);
			changed = true;
		}
		if (!changed) return;
		this.#indexChanged();
		if (entries.length > 0 && this.selectedId !== null) {
			const first = [...entries].sort((a, b) => a.at - b.at)[0];
			this.selectedId = first.id;
			this.#lastSelectedIndex = this.index.indexOf(first.id);
		}
		if (this.attached) this.ensureWindow(this.window);
	}

	patchRow(id: number, patch: (row: ThreadRowDto) => ThreadRowDto): void {
		this.rows.update(id, patch);
	}

	ensureWindow(range: Range): void {
		this.window = range;
		if (range.last < range.first) return;
		const ahead = this.#direction > 0;
		const start = ahead ? range.first : Math.max(0, range.first - PAGE_SIZE);
		const end = ahead ? Math.min(this.count, range.last + 1 + PAGE_SIZE) : range.last + 1;
		this.rows.ensure(this.index.slice(start, end));
	}

	noteScrollDirection(direction: number): void {
		if (direction !== 0) this.#direction = Math.sign(direction);
	}

	scheduleRefetch(): void {
		if (this.#refetchTimer) return;
		this.#refetchTimer = setTimeout(() => {
			this.#refetchTimer = null;
			void this.load();
		}, REFETCH_DEBOUNCE_MS);
	}

	changes(page: ChangesDto): void {
		const next = parseCursor(page.next);
		let touched = false;
		for (const change of page.changes) {
			if (this.#cursor && next?.epoch === this.#cursor.epoch && change.seq <= this.#cursor.seq) {
				continue;
			}
			touched = this.#apply(change) || touched;
		}
		if (touched) this.#indexChanged();
		if (this.attached) this.ensureWindow(this.window);
	}

	async reset(): Promise<string | undefined> {
		this.rows.clear();
		this.#cursor = null;
		return this.load();
	}

	#member(payload: ThreadPayload): boolean | undefined {
		const { account, inner } = viewScope(this.view);
		if (account !== null) {
			if (payload.accountId === undefined) return undefined;
			if (payload.accountId !== account) return false;
		}
		if (inner.startsWith('label:')) {
			return payload.labels?.includes(inner.slice('label:'.length));
		}
		const boxes = payload.mailboxes;
		if (!boxes) return undefined;
		if (inner === 'inbox') {
			if (!this.#nav.loaded) return undefined;
			const inbox = this.#nav.inboxIds;
			return boxes.some((b) => inbox.has(b));
		}
		if (inner.startsWith('mailbox:')) {
			return boxes.includes(Number(inner.slice('mailbox:'.length)));
		}
		return undefined;
	}

	/** Returns whether the index changed. */
	#apply(change: ChangeDto): boolean {
		switch (change.entity) {
			case 'thread':
				return this.#applyThread(change);
			case 'message': {
				const thread = (change.payload as MessagePayload | null)?.threadId;
				if (thread !== undefined) this.rows.invalidate(thread);
				return false;
			}
			case 'mailbox':
				if (change.op.kind === 'bulk') this.scheduleRefetch();
				return false;
			default:
				return false;
		}
	}

	#applyThread(change: ChangeDto): boolean {
		const id = change.id;
		this.rows.invalidate(id);
		const kind = change.op.kind;
		if (kind === 'delete' || kind === 'merged') return this.#remove(id);
		if (kind !== 'upsert') return false;
		const payload = (change.payload ?? {}) as ThreadPayload;
		const member = this.#member(payload);
		if (member === undefined) {
			if (this.index.has(id) || payload.lastDate !== undefined) this.scheduleRefetch();
			return false;
		}
		const wasSelected = this.selectedId === id;
		const removed = this.#remove(id);
		if (!member) return removed;
		if (payload.lastDate === undefined) {
			this.scheduleRefetch();
			return removed;
		}
		const at = insertionPoint(
			this.index.length,
			payload.lastDate,
			(i) => {
				const rowId = this.index.at(i);
				return rowId === undefined ? undefined : this.rows.peek(rowId)?.lastDate;
			},
			[this.window, { first: 0, last: Math.min(this.index.length - 1, PAGE_SIZE - 1) }]
		);
		if (at === null) {
			this.scheduleRefetch();
			return removed;
		}
		this.index.insert(at, id);
		if (wasSelected) this.selectedId = id;
		return true;
	}

	#remove(id: number): boolean {
		const at = this.index.indexOf(id);
		if (at === -1) return false;
		this.index.remove(id);
		if (this.selectedId === id) {
			const next = this.index.at(Math.min(at, this.index.length - 1));
			this.selectedId = next ?? null;
		}
		return true;
	}

	#indexChanged(): void {
		this.count = this.index.length;
		this.indexVersion++;
		if (this.selectedId !== null && !this.index.has(this.selectedId)) {
			const at = Math.min(this.#lastSelectedIndex, this.index.length - 1);
			this.selectedId = this.index.at(at) ?? null;
		}
		const active = this.activeIndex;
		if (active !== -1) this.#lastSelectedIndex = active;
	}
}

export const MAX_CACHED_LISTS = 4;

/** Recently used lists stay alive so going back restores scroll and selection instantly. */
export class ThreadLists {
	readonly #lists = new Map<ViewId, ThreadList>();
	readonly #make: (view: ViewId) => ThreadList;
	/** The list the player last browsed; j/k inside a conversation walk it. */
	current = $state<ThreadList | null>(null);
	/** The route that shows `current`, for `u` and the back arrow. */
	currentPath = $state('/inbox');

	constructor(make: (view: ViewId) => ThreadList) {
		this.#make = make;
	}

	open(view: ViewId, path: string): ThreadList {
		let list = this.#lists.get(view);
		if (list) {
			this.#lists.delete(view);
			void list.load();
		} else {
			list = this.#make(view);
			void list.load();
		}
		this.#lists.set(view, list);
		while (this.#lists.size > MAX_CACHED_LISTS) {
			const [oldest, evicted] = this.#lists.entries().next().value as [ViewId, ThreadList];
			this.#lists.delete(oldest);
			evicted.dispose();
		}
		this.current = list;
		this.currentPath = path;
		return list;
	}

	all(): IterableIterator<ThreadList> {
		return this.#lists.values();
	}

	clear(): void {
		for (const list of this.#lists.values()) list.dispose();
		this.#lists.clear();
		this.current = null;
	}
}
