export const SHELL_CACHE_PREFIX = 'tagmail-shell-';
export const SHELL_PAGE = '/index.html';
export const FALLBACK_URL = '/inbox';

/** Paths the service worker leaves to the network untouched. */
const BYPASS_PREFIXES = ['/api/'];
const BYPASS_PATHS = new Set(['/api', '/service-worker.js', '/healthz']);

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
