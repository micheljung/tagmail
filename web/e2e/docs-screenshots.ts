import { NOW } from './mock/fixture.ts';
import { expect, openInbox, row, test } from './helpers.ts';

const IMAGES = '../docs/images';
const DESKTOP = { width: 1280, height: 800 };
const PHONE = { width: 390, height: 844 };

test.use({ locale: 'en-US', timezoneId: 'UTC' });

test.beforeEach(async ({ page }) => {
	await page.emulateMedia({ reducedMotion: 'reduce' });
	await page.clock.setFixedTime(new Date(NOW * 1000));
});

test.describe('desktop', () => {
	test.use({ viewport: DESKTOP, deviceScaleFactor: 1 });

	test('inbox', async ({ page }) => {
		await openInbox(page);
		await expect(page.locator('.live.on')).toBeVisible();
		await page.screenshot({ path: `${IMAGES}/inbox.png` });
	});

	test('thread', async ({ page }) => {
		await openInbox(page);
		await row(page, 0).click();
		await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
		await expect(page.locator('iframe').first()).toBeVisible();
		await page.screenshot({ path: `${IMAGES}/thread.png` });
	});
});

test.describe('phone', () => {
	test.use({ viewport: PHONE, deviceScaleFactor: 2, isMobile: true, hasTouch: true });

	test('inbox', async ({ page }) => {
		await openInbox(page);
		await page.screenshot({ path: `${IMAGES}/phone-inbox.png` });
	});

	test('inbox, dark', async ({ page }) => {
		await page.emulateMedia({ colorScheme: 'dark' });
		await openInbox(page);
		await page.screenshot({ path: `${IMAGES}/phone-inbox-dark.png` });
	});
});
