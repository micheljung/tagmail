export const SHELL_CACHE_PREFIX = 'tagmail-shell-';
export const SHELL_PAGE = '/index.html';
export const FALLBACK_URL = '/inbox';

/** Paths the service worker leaves to the network untouched. */
const BYPASS_PREFIXES = ['/api/'];
const BYPASS_PATHS = new Set(['/api', '/service-worker.js', '/healthz']);

const SERVER_ERROR = 500;

/** A navigation the server failed to answer gets the cached shell, as offline does. */
export function servesShell(status: number): boolean {
	return status >= SERVER_ERROR;
}

export type FetchStrategy = 'bypass' | 'network-first' | 'cache-first';

export interface FetchInfo {
	method: string;
	url: string;
	mode: string;
}

export function shellCacheName(version: string): string {
	return SHELL_CACHE_PREFIX + version;
}

/**
 * Decides how the service worker answers a request. Only the app shell is
 * ever cached, so anything that may carry mail (`/api/*`) is never touched.
 */
export function fetchStrategy(request: FetchInfo, origin: string): FetchStrategy {
	if (request.method !== 'GET') return 'bypass';
	const url = new URL(request.url);
	if (url.origin !== origin) return 'bypass';
	if (BYPASS_PATHS.has(url.pathname) || BYPASS_PREFIXES.some((p) => url.pathname.startsWith(p)))
		return 'bypass';
	return request.mode === 'navigate' ? 'network-first' : 'cache-first';
}

/** Old shell caches to delete once the current version activates. */
export function staleCaches(keys: readonly string[], current: string): string[] {
	return keys.filter((key) => key.startsWith(SHELL_CACHE_PREFIX) && key !== current);
}

const SCRIPT_SRC = /<script\b[^>]*\bsrc="([^"]+)"/g;
const LINK_HREF = /<link\b[^>]*\bhref="([^"]+)"/g;

/**
 * Same-origin scripts and links the shell page references. The boot script
 * is written after the Vite build, so the build manifest does not list it.
 */
export function shellReferences(html: string, origin: string): string[] {
	const found = new Set<string>();
	for (const pattern of [SCRIPT_SRC, LINK_HREF])
		for (const [, ref] of html.matchAll(pattern)) {
			const url = new URL(ref, origin);
			if (url.origin === origin && !BYPASS_PREFIXES.some((p) => url.pathname.startsWith(p)))
				found.add(url.pathname);
		}
	return [...found];
}

/**
 * Only same-origin relative paths may be opened from a notification. The
 * URL parser drops tabs and newlines and reads a backslash as `/`, so the parsed
 * origin decides, not the raw string.
 */
export function safeAppUrl(url: unknown, origin: string): string {
	if (typeof url !== 'string' || !url.startsWith('/')) return FALLBACK_URL;
	const parsed = new URL(url, origin);
	return parsed.origin === origin ? parsed.pathname + parsed.search + parsed.hash : FALLBACK_URL;
}

export const NOTIFICATION_ICON = '/icons/icon-192.png';
export const NOTIFICATION_BADGE = '/icons/badge-96.png';
const DEFAULT_TAG = 'tagmail';
const PAYLOAD_VERSION = 1;

export interface NotificationSpec {
	title: string;
	options: {
		body: string;
		tag: string;
		renotify: boolean;
		icon: string;
		badge: string;
		data: { url: string };
	};
}

const text = (value: unknown) => (typeof value === 'string' && value !== '' ? value : undefined);
const positiveInt = (value: unknown) =>
	Number.isSafeInteger(value) && (value as number) > 0 ? (value as number) : undefined;

/**
 * The notification for a push payload. Anything unexpected still shows
 * "New mail": Safari revokes the permission after silent pushes. Text
 * only, nothing is rendered as HTML.
 */
export function notificationFor(data: string | null | undefined, origin: string): NotificationSpec {
	let payload: Record<string, unknown> = {};
	try {
		const parsed: unknown = JSON.parse(data ?? '');
		if (parsed && typeof parsed === 'object') payload = parsed as Record<string, unknown>;
	} catch {
		// Malformed payloads fall through to the generic notification.
	}
	const known = payload.v === PAYLOAD_VERSION;
	const kind = known ? payload.kind : undefined;
	const count = positiveInt(payload.count) ?? 1;
	const accountId = positiveInt(payload.accountId);
	const from = text(payload.from);
	const subject = text(payload.subject);
	const url = kind === 'newMail' || kind === 'test' ? payload.url : undefined;
	const spec = (title: string, body: string, tag: string, renotify: boolean): NotificationSpec => ({
		title,
		options: {
			body,
			tag,
			renotify,
			icon: NOTIFICATION_ICON,
			badge: NOTIFICATION_BADGE,
			data: { url: safeAppUrl(url, origin) }
		}
	});
	if (kind === 'test') return spec('Test notification', '', DEFAULT_TAG, false);
	if (kind !== 'newMail') return spec('New mail', '', DEFAULT_TAG, false);
	const tag = accountId === undefined ? DEFAULT_TAG : `inbox-${accountId}`;
	const title = count > 1 ? `${count} new messages` : (from ?? 'New mail');
	return spec(title, subject ?? '', tag, count > 1);
}

export interface AppWindow {
	focus(): Promise<unknown>;
	postMessage(message: unknown): void;
}

export interface AppClients {
	matchAll(options: {
		type: 'window';
		includeUncontrolled: boolean;
	}): Promise<readonly AppWindow[]>;
	openWindow(url: string): Promise<unknown>;
}

/**
 * Opens `url` from a notification. An open window is focused and navigates
 * in-app on a message instead of reloading, so a draft being written
 * survives the tap.
 */
export async function openApp(clients: AppClients, url: string): Promise<void> {
	const [open] = await clients.matchAll({ type: 'window', includeUncontrolled: true });
	if (!open) {
		await clients.openWindow(url);
		return;
	}
	await open.focus();
	open.postMessage({ type: 'navigate', url });
}
