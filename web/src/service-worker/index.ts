import { immutable, assets } from '$app/manifest';
import { version } from '$app/env';
import { self } from '$app/service-worker';
import {
	SHELL_PAGE,
	fetchStrategy,
	notificationFor,
	safeAppUrl,
	shellCacheName,
	shellReferences,
	staleCaches
} from '#lib/pwa/sw.ts';
import { base64url } from '#lib/push/client.ts';

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

self.addEventListener('push', (event) => {
	const { title, options } = notificationFor(event.data?.text(), self.location.origin);
	event.waitUntil(self.registration.showNotification(title, options));
});

/**
 * An open window navigates in-app on a message instead of reloading, so a
 * draft being written survives the tap.
 */
async function openApp(url: string): Promise<void> {
	const [open] = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
	if (!open) {
		await self.clients.openWindow(url);
		return;
	}
	await open.focus();
	open.postMessage({ type: 'navigate', url });
}

self.addEventListener('notificationclick', (event) => {
	event.notification.close();
	const data = event.notification.data as { url?: unknown } | null;
	event.waitUntil(openApp(safeAppUrl(data?.url, self.location.origin)));
});

interface PushSubscriptionChangeEvent extends ExtendableEvent {
	readonly oldSubscription: PushSubscription | null;
}

async function resubscribe(old: PushSubscription | null): Promise<void> {
	const key = old?.options.applicationServerKey;
	if (!key) return;
	const renewed = await self.registration.pushManager.subscribe({
		userVisibleOnly: true,
		applicationServerKey: base64url(key)
	});
	const { keys } = renewed.toJSON();
	await fetch('/api/push/subscriptions', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { 'content-type': 'application/json', 'X-Requested-With': 'tagmail' },
		body: JSON.stringify({ endpoint: renewed.endpoint, keys })
	});
}

self.addEventListener('pushsubscriptionchange', (event) => {
	const change = event as PushSubscriptionChangeEvent;
	change.waitUntil(resubscribe(change.oldSubscription).catch(() => {}));
});
