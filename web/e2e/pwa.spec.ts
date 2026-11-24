import type { Page } from '@playwright/test';
import { VAPID_PUBLIC_KEY } from './mock/push.ts';
import { expect, signIn, test } from './helpers.ts';

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
