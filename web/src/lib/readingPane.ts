import { scopedView, viewForPath, withAccount, type UrlLike } from './views';
import type { ViewId } from './transport';

/** Where `ThreadLists.open` keeps the last list path, so a reload on a thread finds its list. */
export const LIST_PATH_KEY = 'tagmail.listPath';

/** How long a conversation stays open in the reading pane before it counts as read. */
export const AUTO_READ_MS = 3000;

export interface ListTarget {
	view: ViewId;
	path: string;
}

/** The open conversation's id on `/thread/<id>`, else `null`. */
export function threadRouteId(url: UrlLike): number | null {
	const m = /^\/thread\/(\d+)$/.exec(url.pathname);
	if (!m) return null;
	const id = Number(m[1]);
	return Number.isSafeInteger(id) ? id : null;
}

/** Whether the `(lists)` layout shows the list next to a reading pane. */
export function splitShown(split: boolean, url: UrlLike): boolean {
	return split && (url.pathname.startsWith('/thread/') || viewForPath(url) !== null);
}

/**
 * The list the split shows: the route's own list, or on a thread the list the
 * player came from, else the one stored for this tab, else the inbox.
 */
export function listTarget(
	url: UrlLike,
	current: () => ListTarget | null,
	stored: () => string | null,
	account: number | null
): ListTarget {
	const own = viewForPath(url);
	if (own) return own;
	const open = current();
	if (open) return open;
	const path = stored();
	const kept = path === null ? null : viewForPath(path);
	if (kept) return kept;
	return { view: scopedView('inbox', account), path: withAccount('/inbox', account) };
}

export function readStoredPath(): string | null {
	try {
		return sessionStorage.getItem(LIST_PATH_KEY);
	} catch {
		return null;
	}
}

export function storePath(path: string): void {
	try {
		sessionStorage.setItem(LIST_PATH_KEY, path);
	} catch {
		// No session storage (tests, private mode): a reload then falls back to the inbox.
	}
}

/**
 * Whether the list pane, rather than the open conversation, takes an act key
 * (`e`, `#`, `s`, …) on `/thread/*`: when rows are ticked, or when focus is in
 * the list and its highlight is another thread.
 */
export function listOwnsAct(state: {
	checked: number;
	selectedId: number | null;
	routeId: number | null;
	focusInList: boolean;
}): boolean {
	return state.checked > 0 || (state.focusInList && state.selectedId !== state.routeId);
}

/** Whether focus sits in the split's list pane. */
export function focusInListPane(): boolean {
	return !!document.activeElement?.closest('.list-pane');
}

/**
 * Marks an unread conversation read once it has stayed open in the reading
 * pane for `delay` ms; returns the cancel function. `isUnread` is checked
 * again when the timer fires, so a thread read meanwhile is left alone.
 */
export function scheduleAutoRead(
	options: { split: boolean; isUnread: () => boolean; markRead: () => void },
	delay = AUTO_READ_MS
): () => void {
	if (!options.split || !options.isUnread()) return () => {};
	const timer = setTimeout(() => {
		if (options.isUnread()) options.markRead();
	}, delay);
	return () => clearTimeout(timer);
}
