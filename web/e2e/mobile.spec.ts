import type { Locator, Page } from '@playwright/test';
import { PASSWORD, idAt } from './mock/fixture.ts';
import { expect, openInbox, row, signIn, test } from './helpers.ts';

const TOUCH_TARGET = 44;
/** Past the 35 % commit threshold of a row. */
const COMMIT_SHARE = 0.6;
/** Under both the flick distance and the commit share. */
const SHORT_SWIPE_PX = 40;
const VERTICAL_DRAG_PX = 160;

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

test('Back from a deep link opened before signing in goes to the inbox', async ({ page }) => {
	await page.goto(`/thread/${idAt(4)}`);
	await expect(page).toHaveURL(/\/login\?next=/);
	await page.getByLabel('Password').fill(PASSWORD);
	await page.getByRole('button', { name: 'Sign in', exact: true }).click();
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

/**
 * Drags a row with synthetic touch pointer events by `dx`/`dy`; `cancel`
 * ends it the way a browser does when it takes the gesture for scrolling,
 * `click` follows the release with the click a short drag can still get.
 */
async function touchDrag(
	target: Locator,
	dx: number,
	dy: number,
	end: 'release' | 'click' | 'cancel' = 'release'
) {
	await target.evaluate(
		(el, [dx, dy, end]) => {
			const box = el.getBoundingClientRect();
			const x = box.left + box.width / 2;
			const y = box.top + box.height / 2;
			const fire = (type: string, at: number) =>
				el.dispatchEvent(
					new PointerEvent(type, {
						bubbles: true,
						cancelable: true,
						pointerId: 7,
						pointerType: 'touch',
						isPrimary: true,
						clientX: x + dx * at,
						clientY: y + dy * at
					})
				);
			const STEPS = 8;
			fire('pointerdown', 0);
			for (let i = 1; i <= STEPS; i++) fire('pointermove', i / STEPS);
			fire(end === 'cancel' ? 'pointercancel' : 'pointerup', 1);
			if (end === 'click')
				el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
		},
		[dx, dy, end] as const
	);
}

function toast(page: Page, text: string) {
	return page.locator('[data-toast]').filter({ hasText: text });
}

function total(page: Page, n: number) {
	return expect(page.locator('.total')).toHaveText(`${n.toLocaleString('en-US')} conversations`);
}

async function rowWidth(page: Page): Promise<number> {
	return (await row(page, 0).boundingBox())!.width;
}

test.describe('swipe', () => {
	test.beforeEach(async ({ page }) => {
		await openInbox(page);
	});

	test('left archives, and Undo puts the row back in place', async ({ page }) => {
		await touchDrag(row(page, 0).locator('.row'), -(await rowWidth(page)) * COMMIT_SHARE, 0);
		await expect(toast(page, 'Archived 1 conversation')).toBeVisible();
		await total(page, 99_999);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(1)}`);
		await expect(page).toHaveURL(/\/inbox$/);
		await toast(page, 'Archived 1 conversation').getByRole('button', { name: 'Undo' }).tap();
		await total(page, 100_000);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
	});

	test('right moves to Trash, and Undo puts the row back in place', async ({ page }) => {
		await touchDrag(row(page, 1).locator('.row'), (await rowWidth(page)) * COMMIT_SHARE, 0);
		await expect(toast(page, 'Moved 1 conversation to Trash')).toBeVisible();
		await total(page, 99_999);
		await expect(row(page, 1)).toHaveId(`thread-${idAt(2)}`);
		await toast(page, 'Moved 1 conversation to Trash').getByRole('button', { name: 'Undo' }).tap();
		await total(page, 100_000);
		await expect(row(page, 1)).toHaveId(`thread-${idAt(1)}`);
	});

	test('shows what each direction does behind the row', async ({ page }) => {
		const target = row(page, 0).locator('.row');
		const box = (await target.boundingBox())!;
		const y = box.y + box.height / 2;
		const start = box.x + box.width / 2;
		const press = (type: string, x: number) =>
			target.dispatchEvent(type, { pointerId: 9, pointerType: 'touch', clientX: x, clientY: y });
		await press('pointerdown', start);
		await press('pointermove', start - SHORT_SWIPE_PX);
		await expect(row(page, 0).locator('.under.archive')).toHaveText('Archive');
		await press('pointermove', start + SHORT_SWIPE_PX);
		await expect(row(page, 0).locator('.under.delete')).toHaveText('Delete');
		const archive = await row(page, 0)
			.locator('.under')
			.evaluate((el) => getComputedStyle(el).backgroundColor);
		await press('pointermove', start - SHORT_SWIPE_PX);
		const trash = await row(page, 0)
			.locator('.under')
			.evaluate((el) => getComputedStyle(el).backgroundColor);
		expect(archive).not.toBe(trash);
		await press('pointercancel', start);
		await expect(target).not.toHaveAttribute('style', /translateX/);
		await total(page, 100_000);
	});

	test('a short swipe snaps back and does not open the thread', async ({ page }) => {
		const target = row(page, 0).locator('.row');
		await touchDrag(target, -SHORT_SWIPE_PX, 0, 'click');
		await expect(target).not.toHaveAttribute('style', /translateX/);
		await expect(page).toHaveURL(/\/inbox$/);
		await total(page, 100_000);
		await expect(page.locator('[data-toast]')).toHaveCount(0);
	});

	test('a vertical drag leaves the row to scrolling', async ({ page }) => {
		const target = row(page, 2).locator('.row');
		expect(await target.evaluate((el) => getComputedStyle(el).touchAction)).toBe('pan-y');
		await touchDrag(target, -SHORT_SWIPE_PX, -VERTICAL_DRAG_PX, 'cancel');
		await expect(target).not.toHaveAttribute('style', /translateX/);
		await total(page, 100_000);
		await expect(page.locator('[data-toast]')).toHaveCount(0);
	});

	test('a ticked selection does not swipe', async ({ page }) => {
		await page.getByRole('checkbox', { name: 'Select all conversations on screen' }).tap();
		const selected = page.locator('.total');
		await expect(selected).toHaveText(/selected$/);
		const before = await selected.textContent();
		await touchDrag(row(page, 1).locator('.row'), -(await rowWidth(page)) * COMMIT_SHARE, 0);
		await expect(selected).toHaveText(before!);
		await expect(row(page, 1)).toHaveId(`thread-${idAt(1)}`);
		await expect(page.locator('[data-toast]')).toHaveCount(0);
	});

	test('a right swipe in Trash does nothing, never deleting for good', async ({ page }) => {
		await touchDrag(row(page, 0).locator('.row'), (await rowWidth(page)) * COMMIT_SHARE, 0);
		await expect(toast(page, 'Moved 1 conversation to Trash')).toBeVisible();
		await page.goto('/mailbox/4');
		const trash = page.getByRole('listbox', { name: 'Folder Trash' });
		await expect(trash.locator('[data-loaded]').first()).toBeVisible();
		const target = row(page, 0).locator('.row');
		await touchDrag(target, (await rowWidth(page)) * COMMIT_SHARE, 0);
		await expect(target).not.toHaveAttribute('style', /translateX/);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
		await expect(page.locator('.total')).toHaveText('1 conversation');
		await expect(toast(page, 'forever')).toHaveCount(0);
		await expect(page).toHaveURL(/\/mailbox\/4$/);
	});
});
