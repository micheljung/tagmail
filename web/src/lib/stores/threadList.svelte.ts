import type { ChangeDto, ChangesDto, ThreadRowDto } from '@tagmail/api-types';
import { SvelteSet } from 'svelte/reactivity';
import { ThreadIndex, insertionPoint } from '../list/threadIndex';
import { PAGE_SIZE, RowCache } from '../list/rowCache.svelte';
import type { Range } from '../list/math';
import { partition } from '../list/sections';
import type { Api, ViewId, ViewIndexResult } from '../transport';
import { ApiError } from '../transport/api';
import { storePath } from '../readingPane';
import { scopedView, viewScope } from '../views';
import { parseCursor, type Cursor } from './cursor';
import type { ChangeListener, EventHub } from './events.svelte';
import type { NavStore } from './nav.svelte';
import type { OpIntent } from '../ops/effects';
import type { OpList, OpPhase, Removed } from '../ops/ops.svelte';

export const REFETCH_DEBOUNCE_MS = 300;
/** A refresh spins at least this long, so a fast answer does not flicker. */
export const REFRESH_MIN_SPIN_MS = 600;
/** No qualifying change by then: the refresh ends neutrally. */
export const REFRESH_TIMEOUT_MS = 3500;
/** How long "Up to date" / "Couldn't refresh" stays. */
export const UP_TO_DATE_MS = 1500;

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

export interface ThreadListOptions {
	/** Show unread threads first, then the rest (inbox views only). */
	unreadFirst?: boolean;
	/** The applied markRead/markUnread intents, oldest first (`OpsStore.unreadIntents`). */
	unreadIntents?: () => readonly OpIntent[];
	/** Whether a showing or still-sending op involves thread `id` (`OpsStore.touches`). */
	touches?: (id: number) => boolean;
}

/** An unread state the list knows better than the last unread search, from `gen` on. */
interface UnreadOverride {
	unread: boolean;
	gen: number;
}

type UnreadFetch = { req: number; startGen: number } & (
	{ ok: true; result: ViewIndexResult } | { ok: false; error: unknown }
);

const UNREAD_QUERY = 'search:in:inbox is:unread';

function message(e: unknown): string {
	return e instanceof Error ? e.message : String(e);
}

/**
 * Pull-to-refresh: `busy` while waiting for the sync; then `done` ("Up to
 * date"), `failed` or `offline` for a moment; `idle` again. A change that
 * moves the list ends `busy` straight back to `idle`.
 */
export type RefreshState = 'idle' | 'busy' | 'done' | 'failed' | 'offline';

/**
 * One thread list (inbox, a label, a search): its id index, row cache,
 * selection (by thread id, so patches never shift it) and live patching
 * from the change feed.
 *
 * An unread-first inbox keeps the date-ordered index as its base and shows
 * it partitioned: unread threads (the server's unread search, adjusted by
 * known-lagging overrides and the applied read/unread ops), then the rest.
 * A row that turns read while in the unread section is held there until the
 * window loses focus or the list goes off screen.
 */
export class ThreadList implements ChangeListener, OpList {
	readonly view: ViewId;
	readonly rows: RowCache;
	/** Whether unread-first was asked for (it only takes effect in inbox views). */
	readonly unreadFirst: boolean;
	/** Display order: the base itself, or its unread-first partition. */
	index = new ThreadIndex();
	/** Showing sections: unread first, then the rest. */
	sectioned = $state(false);
	/** Threads in the unread section (the first `split` of `index`). */
	split = $state(0);

	indexVersion = $state(0);
	count = $state(0);
	status = $state<ListStatus>('loading');
	error = $state<string | null>(null);
	/** Answered offline from local mail only (desktop search). */
	partial = $state(false);
	selectedId = $state<number | null>(null);
	/** Threads ticked for a bulk action (`x`, checkboxes). */
	readonly checked = new SvelteSet<number>();
	/** Index of the last ticked row, the start of a shift-click range. */
	checkAnchor = -1;
	/** Scroll position in rows (display slots), kept while the list is off screen. */
	scrollRow = 0;
	/** Rows currently rendered; patches only trust dates inside it. */
	window: Range = { first: 0, last: -1 };
	refreshing = $state<RefreshState>('idle');

	readonly #api: Api;
	readonly #hub: EventHub;
	readonly #nav: NavStore;
	readonly #unsubscribe: () => void;
	#cursor: Cursor | null = null;
	#refetchTimer: ReturnType<typeof setTimeout> | null = null;
	#loadedOnce = false;
	#lastSelectedIndex = 0;
	#direction = 1;
	#attachCount = 0;
	#stopFocusWatch: (() => void) | null = null;
	#disposed = false;
	#refreshStartedAt = 0;
	/** Bumped per refresh so a stale request or status read cannot end a newer one. */
	#refreshRun = 0;
	#refreshTimer: ReturnType<typeof setTimeout> | null = null;
	/** Set by `#apply` when a change asks for a reload (not by the window loader). */
	#changeRefetch = false;

	/** Date-ordered ids: what the view index holds, patched live. */
	#base = new ThreadIndex();
	#indexPartial = false;
	/** The unread search of this inbox; `null` for a flat list. */
	readonly #unreadView: ViewId | null;
	readonly #unreadIntents: () => readonly OpIntent[];
	readonly #touches: (id: number) => boolean;
	#serverUnread = new Set<number>();
	#unreadLoaded = false;
	#unreadPartial = false;
	#unreadCursor: Cursor | null = null;
	#unreadError: string | null = null;
	#overrides = new Map<number, UnreadOverride>();
	#gen = 0;
	#unreadTimer: ReturnType<typeof setTimeout> | null = null;
	#unreadInFlight = false;
	#unreadRerun = false;
	#unreadStale = false;
	#cursorRerun = false;
	#unreadRequests = 0;
	#unreadAccepted = 0;
	/** Read rows kept in the unread section for now. */
	readonly #held = new Set<number>();

	constructor(
		view: ViewId,
		api: Api,
		hub: EventHub,
		nav: NavStore,
		options: ThreadListOptions = {}
	) {
		this.view = view;
		this.#api = api;
		this.#hub = hub;
		this.#nav = nav;
		this.unreadFirst = options.unreadFirst ?? false;
		const { account, inner } = viewScope(view);
		this.#unreadView =
			this.unreadFirst && inner === 'inbox' ? scopedView(UNREAD_QUERY, account) : null;
		this.#unreadIntents = options.unreadIntents ?? (() => []);
		this.#touches = options.touches ?? (() => false);
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

	/** Whether a list view shows it; detached lists don't fetch rows. */
	get attached(): boolean {
		return this.#attachCount > 0;
	}

	/** Marks the list as shown until the returned function is called. */
	attach(): () => void {
		this.#attachCount++;
		if (this.#attachCount === 1) {
			this.#watchFocus();
			if (this.#unreadStale) {
				this.#unreadStale = false;
				this.#scheduleUnread();
			}
		}
		let done = false;
		return () => {
			if (done) return;
			done = true;
			this.#attachCount--;
			if (this.#attachCount > 0) return;
			this.#stopFocusWatch?.();
			this.#stopFocusWatch = null;
			this.releaseHeld();
		};
	}

	async load(): Promise<string | undefined> {
		if (!this.#loadedOnce) this.status = 'loading';
		const unread = this.#unreadView ? this.#fetchUnread() : null;
		try {
			const [{ ids, cursor, partial }, fetched] = await Promise.all([
				this.#api.viewIndex(this.view),
				unread
			]);
			this.#base = new ThreadIndex(ids);
			this.#indexPartial = partial;
			this.#cursor = parseCursor(cursor);
			this.#loadedOnce = true;
			if (fetched) this.#takeUnread(fetched);
			this.partial = this.#indexPartial || (this.sectioned && this.#unreadPartial);
			this.#indexChanged();
			if (this.selectedId === null && this.count > 0) this.select(0);
			this.status = 'ready';
			this.error = this.#unreadError;
			if (!this.#hub.running) this.#hub.start(cursor || undefined);
			if (this.attached) this.ensureWindow(this.window);
			return cursor || undefined;
		} catch (e) {
			this.error = message(e);
			if (!this.#loadedOnce) this.status = 'error';
			return undefined;
		}
	}

	dispose(): void {
		this.#disposed = true;
		this.#unsubscribe();
		if (this.#refetchTimer) clearTimeout(this.#refetchTimer);
		if (this.#unreadTimer) clearTimeout(this.#unreadTimer);
		this.#stopFocusWatch?.();
		this.#stopFocusWatch = null;
		this.#held.clear();
		this.#clearRefreshTimer();
		this.#refreshRun++;
		this.rows.clear();
	}

	/** Whether thread `id` counts as unread before the active ops. */
	baseUnread(id: number): boolean {
		return this.#overrides.get(id)?.unread ?? this.#serverUnread.has(id);
	}

	/** Whether thread `id` belongs in the unread section (ignoring holds). */
	member(id: number): boolean {
		return this.#memberOf()(id);
	}

	/** Moves held read rows to where they belong. */
	releaseHeld(): void {
		if (this.#held.size === 0) return;
		this.#held.clear();
		this.#indexChanged(true, false);
		if (this.attached) this.ensureWindow(this.window);
	}

	opsChanged(intent: OpIntent, phase: OpPhase): void {
		if (this.#disposed || !this.#unreadView) return;
		const mine = intent.threadIds.filter((id) => this.#base.has(id));
		if (mine.length === 0) return;
		const flips = intent.kind === 'markRead' || intent.kind === 'markUnread';
		if (phase === 'done') {
			// The search may still lag the confirmed op: keep its effect until a later answer.
			if (flips) {
				const unread = intent.kind === 'markUnread';
				for (const id of mine) this.#overrides.set(id, { unread, gen: ++this.#gen });
			}
			this.#scheduleUnread();
		}
		if (!flips || !this.sectioned) return;
		this.#indexChanged();
		if (this.attached) this.ensureWindow(this.window);
	}

	idAt(index: number): number | undefined {
		return this.index.at(index);
	}

	select(index: number): void {
		const id = this.index.at(index);
		if (id === undefined) return;
		this.#direction = index >= this.#lastSelectedIndex ? 1 : -1;
		this.#setSelected(id);
		this.#lastSelectedIndex = index;
	}

	/** Selects thread `id` if the list has it (the open conversation). */
	selectThread(id: number): void {
		const at = this.index.indexOf(id);
		if (at === -1) return;
		this.#setSelected(id);
		this.#lastSelectedIndex = at;
	}

	#setSelected(id: number | null): void {
		this.selectedId = id;
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

	/** Removes threads; `at` is the base (date order) position, `wasUnread` the base state. */
	takeOut(ids: readonly number[]): Removed[] {
		if (this.#disposed) return [];
		const out: Removed[] = [];
		for (const id of ids) {
			const at = this.#base.indexOf(id);
			if (at === -1) continue;
			out.push(this.#unreadView ? { id, at, wasUnread: this.baseUnread(id) } : { id, at });
		}
		if (out.length === 0) return out;
		out.sort((a, b) => a.at - b.at);
		const gone = new Set(out.map((r) => r.id));
		const selectedAt = this.selectedId === null ? -1 : this.index.indexOf(this.selectedId);
		let before = 0;
		for (const id of gone) if (this.index.indexOf(id) < selectedAt) before++;
		this.#base.removeMany(gone);
		this.#rebuild(true);
		if (this.selectedId !== null && gone.has(this.selectedId)) {
			const at = Math.min(selectedAt - before, this.index.length - 1);
			this.#setSelected(this.index.at(at) ?? null);
			this.#lastSelectedIndex = Math.max(0, at);
		}
		for (const id of gone) this.checked.delete(id);
		this.#indexChanged(false);
		if (this.attached) this.ensureWindow(this.window);
		return out;
	}

	putBack(entries: readonly Removed[]): void {
		if (this.#disposed) return;
		let changed = false;
		const sorted = [...entries].sort((a, b) => a.at - b.at);
		for (const { id, at, wasUnread } of sorted) {
			if (this.#base.has(id)) continue;
			this.#base.insert(Math.min(at, this.#base.length), id);
			if (this.#unreadView && wasUnread !== undefined) {
				this.#overrides.set(id, { unread: wasUnread, gen: ++this.#gen });
			}
			changed = true;
		}
		if (!changed) return;
		this.#indexChanged();
		if (sorted.length > 0 && this.selectedId !== null) {
			this.#setSelected(sorted[0].id);
			this.#lastSelectedIndex = this.index.indexOf(sorted[0].id);
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
		const ids = this.index.slice(start, end);
		// Sectioned, the base's first row anchors the known insertion range for new mail.
		const first = this.#unreadView ? this.#base.at(0) : undefined;
		this.rows.ensure(first === undefined ? ids : [...ids, first]);
	}

	noteScrollDirection(direction: number): void {
		if (direction !== 0) this.#direction = Math.sign(direction);
	}

	/**
	 * Asks the engine to sync the view's accounts now. The sync sends no
	 * completion signal: a change that moves this list ends the spinner early;
	 * otherwise it ends at `REFRESH_TIMEOUT_MS`. A failed sync request ends it
	 * as soon as it fails (after the minimum spin), before or after the timeout.
	 */
	async refresh(): Promise<void> {
		if (this.refreshing === 'busy') return;
		this.#clearRefreshTimer();
		const run = ++this.#refreshRun;
		this.#refreshStartedAt = Date.now();
		this.refreshing = 'busy';
		const sync = this.#api.syncView(this.view).then(
			(): RefreshState | null => null,
			(e: unknown): RefreshState => (isOffline(e) ? 'offline' : 'failed')
		);
		this.#refreshTimer = setTimeout(
			() => void this.#refreshTimedOut(run, sync),
			REFRESH_TIMEOUT_MS
		);
		const failure = await sync;
		if (failure === null || run !== this.#refreshRun || this.refreshing !== 'busy') return;
		this.#endRefresh(failure);
	}

	/**
	 * The timeout: the `/api/status` read starts now, so with the sync request
	 * already answered the refresh ends about one status read after
	 * `REFRESH_TIMEOUT_MS`. A request still pending is waited for: "Up to date"
	 * (or the status's verdict) only once it succeeded, its failure otherwise.
	 */
	async #refreshTimedOut(run: number, sync: Promise<RefreshState | null>): Promise<void> {
		this.#refreshTimer = null;
		const status = this.#statusOutcome();
		const outcome = (await sync) ?? (await status);
		if (run !== this.#refreshRun || this.refreshing !== 'busy') return;
		this.#endRefresh(outcome);
	}

	/** `failed` when an account of this view reports a last error, else `done`. */
	async #statusOutcome(): Promise<RefreshState> {
		try {
			const accounts = this.#viewAccounts();
			const { accounts: status } = await this.#api.status();
			return status.some((a) => a.lastError && (accounts === null || accounts.has(a.id)))
				? 'failed'
				: 'done';
		} catch (e) {
			return isOffline(e) ? 'offline' : 'failed';
		}
	}

	/** Ends `busy` with `outcome`, held until the minimum spin has passed. */
	#endRefresh(outcome: RefreshState): void {
		this.#clearRefreshTimer();
		const run = this.#refreshRun;
		const settle = () => {
			this.#refreshTimer = null;
			if (run !== this.#refreshRun) return;
			this.refreshing = outcome;
			if (outcome === 'idle') return;
			this.#refreshTimer = setTimeout(() => {
				this.#refreshTimer = null;
				if (run === this.#refreshRun) this.refreshing = 'idle';
			}, UP_TO_DATE_MS);
		};
		const wait = this.#refreshStartedAt + REFRESH_MIN_SPIN_MS - Date.now();
		if (wait > 0) this.#refreshTimer = setTimeout(settle, wait);
		else settle();
	}

	#clearRefreshTimer(): void {
		if (this.#refreshTimer) clearTimeout(this.#refreshTimer);
		this.#refreshTimer = null;
	}

	/** Accounts a sync of this view covers, `null` for every account. */
	#viewAccounts(): Set<number> | null {
		const { account, inner } = viewScope(this.view);
		if (account !== null) return new Set([account]);
		if (inner.startsWith('mailbox:')) {
			const id = Number(inner.slice('mailbox:'.length));
			const box = this.#nav.mailboxes.find((m) => m.id === id);
			if (box) return new Set([box.accountId]);
		}
		return null;
	}

	/** A reload requested by a change; it also ends a running refresh. */
	#refetchForChange(): void {
		this.#changeRefetch = true;
		this.scheduleRefetch();
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
		this.#changeRefetch = false;
		for (const change of page.changes) {
			// Before the skip: the index may already have a change the unread search lacks.
			if (this.#unreadView) this.#noteUnreadChange(change);
			if (this.#cursor && next?.epoch === this.#cursor.epoch && change.seq <= this.#cursor.seq) {
				continue;
			}
			touched = this.#apply(change) || touched;
		}
		if (touched) this.#indexChanged();
		if ((touched || this.#changeRefetch) && this.refreshing === 'busy') this.#endRefresh('idle');
		this.#changeRefetch = false;
		if (this.attached) this.ensureWindow(this.window);
	}

	async reset(): Promise<string | undefined> {
		this.releaseHeld();
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
				if (change.op.kind === 'bulk') this.#refetchForChange();
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
			if (this.#base.has(id) || payload.lastDate !== undefined) this.#refetchForChange();
			return false;
		}
		// Sectioned, base dates are only known around the loaded display rows: a thread whose
		// date did not change (a read flip) keeps its place instead of a remove and a refetch.
		if (
			member &&
			this.#unreadView &&
			payload.lastDate !== undefined &&
			this.#base.has(id) &&
			(this.#held.has(id) || this.rows.peek(id)?.lastDate === payload.lastDate)
		) {
			return false;
		}
		const wasSelected = this.selectedId === id;
		const removed = this.#remove(id);
		if (!member) return removed;
		if (payload.lastDate === undefined) {
			this.#refetchForChange();
			return removed;
		}
		const base = this.#base;
		const top = { first: 0, last: Math.min(base.length - 1, PAGE_SIZE - 1) };
		// `window` is in display order; sectioned, only base ranges carry dates in order.
		const known = this.#unreadView
			? [{ first: 0, last: Math.min(0, top.last) }, top]
			: [this.window, top];
		const at = insertionPoint(
			base.length,
			payload.lastDate,
			(i) => {
				const rowId = base.at(i);
				return rowId === undefined ? undefined : this.rows.peek(rowId)?.lastDate;
			},
			known
		);
		if (at === null) {
			this.#refetchForChange();
			return removed;
		}
		base.insert(at, id);
		if (this.#unreadView && !removed) {
			// New mail counts as unread until the unread search says otherwise.
			this.#overrides.set(id, { unread: true, gen: ++this.#gen });
			this.#scheduleUnread();
		}
		if (wasSelected) this.#setSelected(id);
		return true;
	}

	#remove(id: number): boolean {
		const at = this.#base.indexOf(id);
		if (at === -1) return false;
		this.#base.remove(id);
		// Sectioned, `#indexChanged` picks the next row in display order.
		if (this.selectedId === id && !this.#unreadView) {
			const next = this.#base.at(Math.min(at, this.#base.length - 1));
			this.#setSelected(next ?? null);
		}
		return true;
	}

	#indexChanged(rebuild = true, hold = true): void {
		if (rebuild) this.#rebuild(hold);
		this.count = this.index.length;
		this.indexVersion++;
		if (this.selectedId !== null && !this.index.has(this.selectedId)) {
			const at = Math.min(this.#lastSelectedIndex, this.index.length - 1);
			this.#setSelected(this.index.at(at) ?? null);
		}
		const active = this.activeIndex;
		if (active !== -1) this.#lastSelectedIndex = active;
	}

	/** Recomputes the display order; with `hold`, rows that just turned read stay put. */
	#rebuild(hold: boolean): void {
		if (!this.sectioned) {
			this.index = this.#base;
			this.split = 0;
			this.#held.clear();
			return;
		}
		const member = this.#memberOf();
		if (hold && this.attached) {
			for (let i = 0; i < this.split; i++) {
				const id = this.index.at(i);
				if (id !== undefined && !member(id)) this.#held.add(id);
			}
		}
		for (const id of this.#held) {
			if (member(id) || !this.#base.has(id)) this.#held.delete(id);
		}
		const { index, split } = partition(this.#base, member, this.#held);
		this.index = index;
		this.split = split;
	}

	/** Unread membership: the base state projected through the applied read/unread ops. */
	#memberOf(): (id: number) => boolean {
		const projected = new Map<number, boolean>();
		for (const intent of this.#unreadIntents()) {
			for (const id of intent.threadIds) projected.set(id, intent.kind === 'markUnread');
		}
		return (id) => projected.get(id) ?? this.baseUnread(id);
	}

	#noteUnreadChange(change: ChangeDto): void {
		const id =
			change.entity === 'thread'
				? change.id
				: change.entity === 'message'
					? (change.payload as MessagePayload | null)?.threadId
					: undefined;
		if (id !== undefined && this.#base.has(id)) this.#scheduleUnread();
	}

	#scheduleUnread(): void {
		if (!this.#unreadView || this.#disposed) return;
		if (!this.attached) {
			this.#unreadStale = true;
			return;
		}
		if (this.#unreadTimer) return;
		this.#unreadTimer = setTimeout(() => {
			this.#unreadTimer = null;
			void this.#refreshUnread();
		}, REFETCH_DEBOUNCE_MS);
	}

	async #refreshUnread(): Promise<void> {
		if (this.#unreadInFlight) {
			this.#unreadRerun = true;
			return;
		}
		this.#unreadInFlight = true;
		const shown = this.#unreadError;
		const fetched = await this.#fetchUnread();
		this.#unreadInFlight = false;
		if (this.#takeUnread(fetched)) {
			if (shown !== null && this.error === shown) this.error = null;
			this.partial = this.#indexPartial || this.#unreadPartial;
			this.#indexChanged();
			if (this.attached) this.ensureWindow(this.window);
		}
		if (this.#unreadRerun) {
			this.#unreadRerun = false;
			this.#scheduleUnread();
		}
	}

	#fetchUnread(): Promise<UnreadFetch> {
		const req = ++this.#unreadRequests;
		const startGen = this.#gen;
		return this.#api.viewIndex(this.#unreadView!).then(
			(result) => ({ req, startGen, ok: true as const, result }),
			(error: unknown) => ({ req, startGen, ok: false as const, error })
		);
	}

	/** Takes an unread search answer; returns whether membership changed. */
	#takeUnread(fetched: UnreadFetch): boolean {
		if (this.#disposed || fetched.req < this.#unreadAccepted) return false;
		if (!fetched.ok) {
			// Later failures keep the last answer; the first one leaves the list flat.
			if (!this.#unreadLoaded) this.#unreadError = message(fetched.error);
			return false;
		}
		this.#unreadAccepted = fetched.req;
		this.#serverUnread = new Set(fetched.result.ids);
		this.#unreadPartial = fetched.result.partial;
		this.#unreadLoaded = true;
		this.#unreadError = null;
		for (const [id, o] of this.#overrides) {
			if (fetched.startGen >= o.gen && !this.#touches(id)) this.#overrides.delete(id);
		}
		// The unread answer predates changes the index has (and the feed skips): ask once more.
		this.#unreadCursor = parseCursor(fetched.result.cursor);
		const [mine, index] = [this.#unreadCursor, this.#cursor];
		const lags =
			mine !== null && index !== null && (mine.epoch !== index.epoch || mine.seq < index.seq);
		if (lags && !this.#cursorRerun) {
			this.#cursorRerun = true;
			this.#scheduleUnread();
		} else this.#cursorRerun = false;
		this.sectioned = true;
		return true;
	}

	/** Leaving the tab or window lets held rows move. */
	#watchFocus(): void {
		if (!this.#unreadView) return;
		const doc = typeof document === 'undefined' ? null : document;
		const win = typeof window === 'undefined' ? null : window;
		const onVisibility = () => {
			if (doc?.visibilityState === 'hidden') this.releaseHeld();
		};
		// Focus moving into a message iframe blurs the window too, but the document keeps focus.
		const onBlur = () =>
			setTimeout(() => {
				if (!doc?.hasFocus()) this.releaseHeld();
			}, 0);
		doc?.addEventListener('visibilitychange', onVisibility);
		win?.addEventListener('blur', onBlur);
		this.#stopFocusWatch = () => {
			doc?.removeEventListener('visibilitychange', onVisibility);
			win?.removeEventListener('blur', onBlur);
		};
	}
}

function isOffline(e: unknown): boolean {
	if (typeof navigator !== 'undefined' && navigator.onLine === false) return true;
	return e instanceof TypeError || (e instanceof ApiError && e.code === 'offline');
}

export const MAX_CACHED_LISTS = 4;

/** Recently used lists stay alive so going back restores scroll and selection instantly. */
export class ThreadLists {
	readonly #lists = new Map<ViewId, ThreadList>();
	readonly #make: (view: ViewId, options: { unreadFirst: boolean }) => ThreadList;
	/** The list the player last browsed; j/k inside a conversation walk it. */
	current = $state<ThreadList | null>(null);
	/** The route that shows `current`, for `u` and the back arrow. */
	currentPath = $state('/inbox');

	constructor(make: (view: ViewId, options: { unreadFirst: boolean }) => ThreadList) {
		this.#make = make;
	}

	open(view: ViewId, path: string, options: { unreadFirst?: boolean } = {}): ThreadList {
		const unreadFirst = options.unreadFirst ?? false;
		let list = this.#lists.get(view);
		if (list && list.unreadFirst !== unreadFirst) {
			// Another inbox type: a fresh list (live ops of the old one become no-ops).
			this.#lists.delete(view);
			list.dispose();
			list = undefined;
		}
		if (list) {
			this.#lists.delete(view);
			void list.load();
		} else {
			list = this.#make(view, { unreadFirst });
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
		storePath(path);
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
