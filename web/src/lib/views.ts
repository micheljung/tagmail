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
