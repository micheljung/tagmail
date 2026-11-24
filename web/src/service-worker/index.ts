import { immutable, assets } from '$app/manifest';
import { version } from '$app/env';
import { self } from '$app/service-worker';
import {
	SHELL_PAGE,
	fetchStrategy,
	shellCacheName,
	shellReferences,
	staleCaches
} from '#lib/pwa/sw.ts';

const CACHE = shellCacheName(version);

async function precache(): Promise<void> {
	const cache = await caches.open(CACHE);
	await cache.addAll([...immutable, ...assets].map(({ path }) => `/${path}`));
	const page = await fetch(SHELL_PAGE, { cache: 'no-cache' });
	if (!page.ok) throw new Error(`${SHELL_PAGE} answered ${page.status}`);
	const html = await page.clone().text();
	await cache.put(SHELL_PAGE, page);
	await cache.addAll(shellReferences(html, self.location.origin));
}

async function networkFirst(request: Request): Promise<Response> {
	try {
		return await fetch(request);
	} catch (error) {
		const shell = await caches.match(SHELL_PAGE, { cacheName: CACHE });
		if (shell) return shell;
		throw error;
	}
}

async function cacheFirst(request: Request): Promise<Response> {
	return (await caches.match(request, { cacheName: CACHE })) ?? fetch(request);
}

self.addEventListener('install', (event) => event.waitUntil(precache()));

self.addEventListener('activate', (event) =>
	event.waitUntil(
		(async () => {
			await Promise.all(staleCaches(await caches.keys(), CACHE).map((key) => caches.delete(key)));
			await self.clients.claim();
		})()
	)
);

self.addEventListener('fetch', (event) => {
	const strategy = fetchStrategy(event.request, self.location.origin);
	if (strategy === 'network-first') event.respondWith(networkFirst(event.request));
	else if (strategy === 'cache-first') event.respondWith(cacheFirst(event.request));
});

self.addEventListener('message', (event) => {
	if ((event.data as { type?: unknown } | null)?.type === 'skipWaiting') void self.skipWaiting();
});
