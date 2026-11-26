import type { ScopableView, ViewId } from './transport';

/** The URL parameter that scopes `/inbox`, `/label/…` and `/search` to one account. */
export const ACCOUNT_PARAM = 'account';

const SCOPED = /^account:(\d+)\/(.+)$/;

export function scopedView(view: ScopableView, account: number | null): ViewId {
	return account === null ? view : `account:${account}/${view}`;
}

/** The account a view is limited to (`null`: every account) and the view inside the scope. */
export function viewScope(view: ViewId): { account: number | null; inner: ViewId } {
	const m = SCOPED.exec(view);
	return m ? { account: Number(m[1]), inner: m[2] as ViewId } : { account: null, inner: view };
}

export function accountParam(url: {
	readonly searchParams: { get(name: string): string | null };
}): number | null {
	const raw = url.searchParams.get(ACCOUNT_PARAM);
	const id = raw === null ? NaN : Number(raw);
	return Number.isSafeInteger(id) && id > 0 ? id : null;
}

/** `href` with the account scope appended (or left unscoped for `null`). */
export function withAccount(href: string, account: number | null): string {
	if (account === null) return href;
	return `${href}${href.includes('?') ? '&' : '?'}${ACCOUNT_PARAM}=${account}`;
}

/** The parts of a URL the list mapping reads (`page.url` is a read-only URL). */
export interface UrlLike {
	readonly pathname: string;
	readonly searchParams: { get(name: string): string | null };
}

/** The view and list path a URL shows, or `null` when it is not a thread list. */
export function viewForPath(input: string | UrlLike): { view: ViewId; path: string } | null {
	const url = typeof input === 'string' ? new URL(input, 'http://tagmail.invalid') : input;
	const account = accountParam(url);
	const parts = url.pathname.split('/').filter(Boolean);
	if (parts[0] === 'inbox' && parts.length === 1) {
		return { view: scopedView('inbox', account), path: withAccount('/inbox', account) };
	}
	if (parts[0] === 'label' && parts.length === 2) {
		let key: string;
		try {
			key = decodeURIComponent(parts[1]);
		} catch {
			return null;
		}
		return {
			view: scopedView(`label:${key}`, account),
			path: withAccount(`/label/${encodeURIComponent(key)}`, account)
		};
	}
	if (parts[0] === 'mailbox' && parts.length === 2) {
		const id = Number(parts[1]);
		if (!Number.isSafeInteger(id)) return null;
		return { view: `mailbox:${id}`, path: `/mailbox/${id}` };
	}
	if (parts[0] === 'search' && parts.length === 1) {
		const query = (url.searchParams.get('q') ?? '').trim();
		if (!query) return null;
		return {
			view: scopedView(`search:${query}`, account),
			path: withAccount(`/search?q=${encodeURIComponent(query)}`, account)
		};
	}
	return null;
}
