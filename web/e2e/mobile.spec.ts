import type { Locator } from '@playwright/test';
import { idAt } from './mock/fixture.ts';
import { expect, openInbox, row, signIn, test } from './helpers.ts';

const TOUCH_TARGET = 44;

async function expectTouchSize(target: Locator): Promise<void> {
	const box = await target.boundingBox();
	expect(box, 'rendered').not.toBeNull();
	expect(box!.width).toBeGreaterThanOrEqual(TOUCH_TARGET);
	expect(box!.height).toBeGreaterThanOrEqual(TOUCH_TARGET);
}

test.describe('drawer', () => {
	test.beforeEach(async ({ page }) => {
		await openInbox(page);
	});

	test('opens from the menu and keeps focus inside', async ({ page }) => {
		const menu = page.getByRole('button', { name: 'Menu', exact: true });
		const drawer = page.getByRole('dialog', { name: 'Menu', exact: true });
		await expect(menu).toHaveAttribute('aria-expanded', 'false');
		await expect(page.locator('#nav')).not.toBeInViewport();
		await menu.tap();
		await expect(menu).toHaveAttribute('aria-expanded', 'true');
		await expect(drawer).toBeInViewport();
		await expect(drawer.getByRole('button', { name: 'Compose' })).toBeFocused();
		for (let i = 0; i < 40; i++) {
			await page.keyboard.press('Tab');
			expect(await drawer.evaluate((el) => el.contains(document.activeElement))).toBe(true);
		}
	});

	test('closes on Escape and gives focus back to the menu', async ({ page }) => {
		const menu = page.getByRole('button', { name: 'Menu', exact: true });
		await menu.tap();
		await expect(page.getByRole('dialog', { name: 'Menu', exact: true })).toBeInViewport();
		await page.keyboard.press('Escape');
		await expect(menu).toHaveAttribute('aria-expanded', 'false');
		await expect(menu).toBeFocused();
		await expect(page.locator('#nav')).not.toBeInViewport();
	});

	test('closes on the backdrop', async ({ page }) => {
		const menu = page.getByRole('button', { name: 'Menu', exact: true });
		await menu.tap();
		const viewport = page.viewportSize()!;
		await page.touchscreen.tap(viewport.width - 10, viewport.height / 2);
		await expect(menu).toHaveAttribute('aria-expanded', 'false');
		await expect(page.locator('#nav')).not.toBeInViewport();
	});

	test('closes when the player navigates', async ({ page }) => {
		await page.getByRole('button', { name: 'Menu', exact: true }).tap();
		await page.getByRole('link', { name: 'Starred' }).tap();
		await expect(page).toHaveURL(/\/search\?q=is%3Astarred$/);
		await expect(page.getByRole('button', { name: 'Menu', exact: true })).toHaveAttribute(
			'aria-expanded',
			'false'
		);
		await expect(page.locator('#nav')).not.toBeInViewport();
	});
});

test('a conversation opens full screen and Back returns to the list', async ({ page }) => {
	await openInbox(page);
	const subject = await row(page, 2).locator('.subject').textContent();
	await row(page, 2).locator('.subject').tap();
	await expect(page).toHaveURL(/\/thread\/\d+$/);
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(subject!);
	const viewport = page.viewportSize()!;
	const box = (await page.locator('article.thread').boundingBox())!;
	expect(box.x).toBe(0);
	expect(box.width).toBe(viewport.width);
	await page.getByRole('button', { name: 'Back' }).tap();
	await expect(page).toHaveURL(/\/inbox$/);
	await expect(row(page, 2).locator('.subject')).toHaveText(subject!);
});

test('Back from a deep link goes to the inbox', async ({ page }) => {
	await signIn(page);
	await page.goto(`/thread/${idAt(4)}`);
	await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
	await page.getByRole('button', { name: 'Back' }).tap();
	await expect(page).toHaveURL(/\/inbox$/);
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
});

test('compose opens as a full-screen sheet', async ({ page }) => {
	await openInbox(page);
	await page.getByRole('button', { name: 'Menu', exact: true }).tap();
	await page.getByRole('button', { name: 'Compose' }).tap();
	const sheet = page.getByRole('region', { name: /^Compose/ });
	await expect(sheet).toBeVisible();
	const viewport = page.viewportSize()!;
	await expect(async () => {
		const box = (await sheet.boundingBox())!;
		expect([box.x, box.y, box.width, Math.round(box.height)]).toEqual([
			0,
			0,
			viewport.width,
			viewport.height
		]);
	}).toPass();
	await expectTouchSize(sheet.getByRole('button', { name: 'Save and close' }));
});

test('touch targets are at least 44 px', async ({ page }) => {
	await openInbox(page);
	await expectTouchSize(page.getByRole('button', { name: 'Menu', exact: true }));
	await expectTouchSize(row(page, 0).getByRole('button', { name: /starred/i }));
	await page.getByRole('button', { name: 'Menu', exact: true }).tap();
	const drawer = page.getByRole('dialog', { name: 'Menu', exact: true });
	await expect(drawer).toBeInViewport();
	for (const name of ['Inbox', 'Starred', 'Sent', 'Accounts']) {
		await expectTouchSize(drawer.getByRole('link', { name, exact: true }));
	}
	await expectTouchSize(drawer.getByRole('button', { name: 'Keyboard shortcuts' }));
	await page.keyboard.press('Escape');
	await row(page, 0).locator('.subject').tap();
	await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
	await expectTouchSize(page.getByRole('button', { name: 'Back' }));
	for (const name of ['Archive', 'Delete', 'Mark as unread', 'Label as', 'Move to']) {
		await expectTouchSize(page.getByRole('button', { name, exact: true }));
	}
	await expectTouchSize(page.getByRole('button', { name: 'Reply', exact: true }));
});
