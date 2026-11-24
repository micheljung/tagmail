import type { BrowserContext, Page, Worker } from '@playwright/test';
import { VAPID_PUBLIC_KEY } from './mock/push.ts';
import { expect, openInbox, row, signIn, test } from './helpers.ts';

const ENDPOINT = 'https://fcm.googleapis.com/fcm/send/e2e-device';
const IPHONE_UA =
	'Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1';

interface PushStub {
	subscribed: number;
	unsubscribed: number;
	permissionRequests: number;
}

interface MockPush {
	posted: { endpoint: string; keys: { p256dh: string; auth: string } }[];
	tests: number[];
	subscriptions: { id: number; preview: boolean; current: boolean }[];
}

/**
 * Replaces the browser's push service, which headless Chromium cannot
 * reach, with a fixed subscription, and the notification permission, which
 * headless Chromium always denies. `existing` starts subscribed with that
 * server key and permission granted.
 */
async function stubPush(page: Page, options: { existing?: string } = {}) {
	await page.addInitScript(
		({ endpoint, existing }) => {
			const stub = { subscribed: 0, unsubscribed: 0, permissionRequests: 0 };
			(window as unknown as { __push: typeof stub }).__push = stub;
			const bytes = (key: string) =>
				Uint8Array.from(atob(key.replaceAll('-', '+').replaceAll('_', '/')), (c) => c.charCodeAt(0))
					.buffer;
			const subscription = (key: ArrayBuffer) => ({
				endpoint,
				options: { applicationServerKey: key, userVisibleOnly: true },
				toJSON: () => ({ endpoint, keys: { p256dh: 'BPk3x0', auth: 'c2VjcmV0LWF1dGgtMTY' } }),
				unsubscribe: () => {
					stub.unsubscribed++;
					current = null;
					return Promise.resolve(true);
				}
			});
			let current: ReturnType<typeof subscription> | null = existing
				? subscription(bytes(existing))
				: null;
			PushManager.prototype.subscribe = function (opts?: PushSubscriptionOptionsInit) {
				stub.subscribed++;
				current = subscription(bytes(opts?.applicationServerKey as string));
				return Promise.resolve(current as unknown as PushSubscription);
			};
			PushManager.prototype.getSubscription = () =>
				Promise.resolve(current as unknown as PushSubscription | null);
			let permission: NotificationPermission = existing ? 'granted' : 'default';
			Object.defineProperty(Notification, 'permission', { get: () => permission });
			Notification.requestPermission = () => {
				stub.permissionRequests++;
				permission = 'granted';
				return Promise.resolve(permission);
			};
		},
		{ endpoint: ENDPOINT, existing: options.existing }
	);
}

const MANIFEST_ICONS = [
	{ src: '/icons/icon-192.png', sizes: '192x192', type: 'image/png' },
	{ src: '/icons/icon-512.png', sizes: '512x512', type: 'image/png' },
	{ src: '/icons/maskable-512.png', sizes: '512x512', type: 'image/png', purpose: 'maskable' }
];

async function controlled(page: Page) {
	await page.evaluate(() => navigator.serviceWorker.ready.then(() => undefined));
	await page.reload();
	await expect
		.poll(() => page.evaluate(() => navigator.serviceWorker.controller !== null))
		.toBe(true);
}

async function cachedPaths(page: Page): Promise<string[]> {
	return page.evaluate(async () => {
		const paths: string[] = [];
		for (const name of await caches.keys())
			for (const request of await (await caches.open(name)).keys())
				paths.push(new URL(request.url).pathname);
		return paths;
	});
}

interface Shown {
	title: string;
	options: { body: string; tag: string; renotify: boolean; data: { url: string } };
}

/**
 * Headless Chromium denies notifications, so `showNotification` would
 * reject; the worker records what it was asked to show instead.
 */
async function recordNotifications(context: BrowserContext): Promise<Worker> {
	const worker = context.serviceWorkers()[0] ?? (await context.waitForEvent('serviceworker'));
	await worker.evaluate(() => {
		const scope = self as unknown as {
			__shown: unknown[];
			registration: ServiceWorkerRegistration;
		};
		scope.__shown = [];
		scope.registration.showNotification = (title, options) => {
			scope.__shown.push({ title, options });
			return Promise.resolve();
		};
	});
	return worker;
}

const stubState = (page: Page) =>
	page.evaluate(() => (window as unknown as { __push: PushStub }).__push);

async function mockPush(page: Page): Promise<MockPush> {
	return (await (await page.request.get('/__mock/push')).json()) as MockPush;
}

async function openSettings(page: Page) {
	await signIn(page);
	await page.goto('/inbox');
	await page.evaluate(() => navigator.serviceWorker.ready.then(() => undefined));
	await page.goto('/settings/notifications');
	await expect(page.getByRole('heading', { name: 'Notifications' })).toBeVisible();
}

test('Enable notifications subscribes this browser and the settings apply to it', async ({
	page
}) => {
	await stubPush(page);
	await page.request.post('/__mock/push/other-device');
	await openSettings(page);

	const enable = page.getByRole('button', { name: 'Enable notifications' });
	await expect(enable).toBeVisible();
	expect((await stubState(page)).permissionRequests).toBe(0);
	await enable.click();

	await expect(page.locator('[data-push-state="enabled"]')).toBeVisible();
	expect(await stubState(page)).toMatchObject({ permissionRequests: 1, subscribed: 1 });
	const { posted } = await mockPush(page);
	expect(posted).toEqual([
		{ endpoint: ENDPOINT, keys: { p256dh: 'BPk3x0', auth: 'c2VjcmV0LWF1dGgtMTY' } }
	]);

	await page.getByLabel('Show sender and subject').uncheck();
	await expect
		.poll(async () => (await mockPush(page)).subscriptions.find((s) => s.current)?.preview)
		.toBe(false);

	await page.getByRole('button', { name: 'Send test notification' }).click();
	await expect.poll(async () => (await mockPush(page)).tests).toHaveLength(1);

	const other = page.locator('[data-push-device]');
	await expect(other).toContainText('Chrome on Android');
	await expect(other).toContainText('fcm.googleapis.com');
	await other.getByRole('button', { name: 'Remove' }).click();
	await expect(other).toHaveCount(0);

	await page.getByRole('button', { name: 'Turn off on this device' }).click();
	await expect(enable).toBeVisible();
	expect((await stubState(page)).unsubscribed).toBe(1);
	expect((await mockPush(page)).subscriptions).toEqual([]);
});

test('an existing subscription is registered again when the app starts', async ({ page }) => {
	await stubPush(page, { existing: VAPID_PUBLIC_KEY });
	await signIn(page);
	await page.goto('/inbox');
	await expect
		.poll(async () => (await mockPush(page)).posted.map((p) => p.endpoint))
		.toEqual([ENDPOINT]);
	expect((await stubState(page)).unsubscribed).toBe(0);
});

test('re-registering after the server lost the subscription keeps its settings', async ({
	page
}) => {
	await stubPush(page, { existing: VAPID_PUBLIC_KEY });
	await openSettings(page);
	await expect(page.locator('[data-push-state="enabled"]')).toBeVisible();
	await page.getByLabel('Show sender and subject').uncheck();
	await expect.poll(async () => (await mockPush(page)).subscriptions[0]?.preview).toBe(false);

	const [lost] = (await mockPush(page)).subscriptions;
	await page.request.delete(`/api/push/subscriptions/${lost.id}`, {
		headers: { 'X-Requested-With': 'tagmail' }
	});
	await page.reload();
	await expect
		.poll(async () => (await mockPush(page)).subscriptions)
		.toMatchObject([{ preview: false }]);
	await expect(page.getByLabel('Show sender and subject')).not.toBeChecked();
});

test('a subscription made with an old server key is replaced', async ({ page }) => {
	await stubPush(page, { existing: 'BAECAwQ' });
	await signIn(page);
	await page.goto('/inbox');
	await expect.poll(async () => (await mockPush(page)).posted).toHaveLength(1);
	expect(await stubState(page)).toMatchObject({ unsubscribed: 1, subscribed: 1 });
});

test('signing out unsubscribes this browser first', async ({ page }) => {
	await stubPush(page, { existing: VAPID_PUBLIC_KEY });
	await signIn(page);
	await page.goto('/inbox');
	await expect.poll(async () => (await mockPush(page)).posted).toHaveLength(1);
	await page.getByRole('button', { name: 'Sign out' }).click();
	await expect(page).toHaveURL(/\/login/);
	expect((await stubState(page)).unsubscribed).toBe(1);
});

test('Settings opened as the first page finds the service worker', async ({ page }) => {
	await stubPush(page);
	await signIn(page);
	await page.goto('/settings/notifications');
	await expect(page.getByRole('button', { name: 'Enable notifications' })).toBeVisible();
	await expect(page.locator('[data-push-state="unsupported"]')).toHaveCount(0);
});

test('notifications blocked in the browser say so', async ({ page }) => {
	await page.addInitScript(() =>
		Object.defineProperty(Notification, 'permission', { get: () => 'denied' })
	);
	await openSettings(page);
	await expect(page.locator('[data-push-state="blocked"]')).toHaveText(
		'Blocked in browser settings'
	);
	await expect(page.getByRole('button', { name: 'Enable notifications' })).toHaveCount(0);
});

test.describe('in an iPhone Safari tab', () => {
	test.use({ userAgent: IPHONE_UA });

	test('Settings asks to add Tagmail to the Home Screen first', async ({ page }) => {
		await page.addInitScript(() => {
			delete (window as unknown as { PushManager?: unknown }).PushManager;
		});
		await openSettings(page);
		await expect(page.locator('[data-push-state="ios-tab"]')).toHaveText(
			'On iPhone and iPad, add Tagmail to your Home Screen first: Share → Add to Home Screen, then open it from there.'
		);
	});
});

test('the app is installable from its manifest', async ({ page, context }) => {
	await signIn(page);
	await page.goto('/inbox');
	await page.evaluate(() => navigator.serviceWorker.ready.then(() => undefined));
	const cdp = await context.newCDPSession(page);
	expect(await cdp.send('Page.getInstallabilityErrors')).toEqual({ installabilityErrors: [] });

	const response = await page.request.get('/manifest.webmanifest');
	expect(response.headers()['content-type']).toBe('application/manifest+json');
	expect(await response.json()).toEqual({
		id: '/',
		name: 'Tagmail',
		short_name: 'Tagmail',
		start_url: '/inbox',
		scope: '/',
		display: 'standalone',
		background_color: '#ffffff',
		theme_color: '#2350c8',
		description: 'Mail with labels for your IMAP accounts',
		icons: MANIFEST_ICONS
	});
	for (const { src } of MANIFEST_ICONS) {
		const icon = await page.request.get(src);
		expect(icon.headers()['content-type'], src).toBe('image/png');
	}
});

test('the service worker controls the app and never caches the API', async ({ page }) => {
	await openInbox(page);
	await controlled(page);
	await row(page, 0).click();
	await page.keyboard.press('Enter');
	await expect(page).toHaveURL(/\/thread\//);
	await expect(page.locator('main')).toContainText(/\S/);

	const paths = await cachedPaths(page);
	expect(paths).toContain('/index.html');
	expect(paths.some((p) => /^\/_app\/boot\.[0-9a-f]+\.js$/.test(p))).toBe(true);
	expect(paths.filter((p) => p.startsWith('/api'))).toEqual([]);
});

test('offline, the app shell still loads and says so, and recovers online', async ({
	page,
	context
}) => {
	await stubPush(page, { existing: VAPID_PUBLIC_KEY });
	await openInbox(page);
	await controlled(page);
	await expect.poll(async () => (await mockPush(page)).posted.length).toBeGreaterThan(0);
	const synced = (await mockPush(page)).posted.length;
	await context.setOffline(true);
	await page.reload();
	await expect(page.locator('[data-offline]')).toHaveText(
		"You're offline. Mail will load when you're back online."
	);
	await expect(page.getByRole('link', { name: 'Inbox', exact: true })).toBeVisible();
	await context.setOffline(false);
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
	await expect(page.locator('[data-offline]')).toHaveCount(0);
	await expect.poll(async () => (await mockPush(page)).posted.length).toBe(synced + 1);
});

test('a push shows its notification, without preview and when malformed too', async ({
	page,
	context
}) => {
	await signIn(page);
	await page.goto('/inbox');
	await page.evaluate(() => navigator.serviceWorker.ready.then(() => undefined));
	const worker = await recordNotifications(context);
	const cdp = await context.newCDPSession(page);
	const registrations: string[] = [];
	cdp.on('ServiceWorker.workerRegistrationUpdated', (e) =>
		registrations.push(...e.registrations.map((r) => r.registrationId))
	);
	await cdp.send('ServiceWorker.enable');
	await expect.poll(() => registrations.length).toBeGreaterThan(0);
	const deliver = (data: string) =>
		cdp.send('ServiceWorker.deliverPushMessage', {
			origin: new URL(page.url()).origin,
			registrationId: registrations[0],
			data
		});

	await deliver(
		JSON.stringify({
			v: 1,
			kind: 'newMail',
			accountId: 1,
			count: 1,
			threadId: 4711,
			from: 'Alice Example',
			subject: 'Lunch?',
			url: '/thread/4711'
		})
	);
	await deliver(JSON.stringify({ v: 1, kind: 'newMail', accountId: 1, count: 1, url: '/inbox' }));
	await deliver('{"v":1,"kind":');

	const shown = () => worker.evaluate(() => (self as unknown as { __shown: Shown[] }).__shown);
	await expect.poll(async () => (await shown()).length).toBe(3);
	const summary = (await shown()).map(({ title, options }) => ({
		title,
		body: options.body,
		tag: options.tag,
		url: options.data.url
	}));
	expect(summary).toEqual([
		{ title: 'Alice Example', body: 'Lunch?', tag: 'inbox-1', url: '/thread/4711' },
		{ title: 'New mail', body: '', tag: 'inbox-1', url: '/inbox' },
		{ title: 'New mail', body: '', tag: 'tagmail', url: '/inbox' }
	]);
});
