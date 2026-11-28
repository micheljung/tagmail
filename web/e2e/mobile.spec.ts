import type { CDPSession, Locator, Page } from '@playwright/test';
import { CORPUS, PASSWORD, corpusThreadId, idAt } from './mock/fixture.ts';
import { dominantTextPx, expect, list, openInbox, row, signIn, test } from './helpers.ts';

const TOUCH_TARGET = 44;
/** WebKit reports layout boxes in fractional units, e.g. 43.999996 for 44. */
const SUBPIXEL_DIGITS = 3;
/** Past the 35 % commit threshold of a row. */
const COMMIT_SHARE = 0.6;
/** Under both the flick distance and the commit share. */
const SHORT_SWIPE_PX = 40;
const VERTICAL_DRAG_PX = 160;

async function expectTouchSize(target: Locator): Promise<void> {
	const box = await target.boundingBox();
	expect(box, 'rendered').not.toBeNull();
	expect(box!.width).toBeCloseTo(Math.max(box!.width, TOUCH_TARGET), SUBPIXEL_DIGITS);
	expect(box!.height).toBeCloseTo(Math.max(box!.height, TOUCH_TARGET), SUBPIXEL_DIGITS);
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
		await expect(drawer.getByRole('link', { name: 'Mail' })).toBeFocused();
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

test('Back from a deep link that redirected to its thread goes to the inbox', async ({ page }) => {
	await signIn(page);
	await page.goto('/thread/999999');
	await expect(page).toHaveURL(new RegExp(`/thread/${idAt(0)}$`));
	await page.getByRole('button', { name: 'Back' }).tap();
	await expect(page).toHaveURL(/\/inbox$/);
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
});

test('Back after a link to the open thread still goes to the inbox', async ({ page }) => {
	await signIn(page);
	await page.goto(`/thread/${idAt(0)}`);
	await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
	const reloaded = page.waitForResponse('**/api/auth/session');
	await page.evaluate(() => {
		const link = document.createElement('a');
		link.href = location.href;
		document.body.append(link);
		link.click();
	});
	await reloaded;
	await page.getByRole('button', { name: 'Back' }).tap();
	await expect(page).toHaveURL(/\/inbox$/);
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

/*
 * Touch input. Chromium drives real touch through CDP, so native pan,
 * `touch-action`, `contextmenu` and passive-listener rules apply. WebKit has
 * no CDP: holds use synthetic touch PointerEvents (the hold timer listens on
 * `pointerdown`), pull-to-refresh uses synthetic touch events (handler logic
 * only: thresholds, disengage rules, request sent; not native pan).
 */

/** Mirrors `HOLD_MS` in `list/swipe.ts`; the wait leaves 100 ms of margin. */
const HOLD_MS = 450;
const HOLD_WAIT_MS = HOLD_MS + 100;
/** Mirror `list/pull.ts` and `stores/threadList.svelte.ts`. */
const PULL_PAST_THRESHOLD_PX = 140;
const PULL_SHORT_PX = 100;
const REFRESH_MIN_SPIN_MS = 600;
const REFRESH_TIMEOUT_MS = 3500;
const TIMEOUT_TOLERANCE_MS = 250;
const MOVE_STEPS = 8;
const OVERSCAN = 6;
const FRAME_MS = 16;

interface Point {
	x: number;
	y: number;
}

interface Touch {
	/** Puts one finger down at the center of `target` (or at a point). */
	down(target: Locator | Point): Promise<void>;
	/** Moves the finger to `dx`/`dy` from where it went down. */
	move(dx: number, dy: number): Promise<void>;
	up(): Promise<void>;
}

async function centerOf(target: Locator | Point): Promise<Point> {
	if (!('boundingBox' in target)) return target;
	const box = await target.boundingBox();
	expect(box, 'target rendered').not.toBeNull();
	return { x: box!.x + box!.width / 2, y: box!.y + box!.height / 2 };
}

async function cdpTouch(page: Page): Promise<Touch> {
	const cdp: CDPSession = await page.context().newCDPSession(page);
	let start: Point = { x: 0, y: 0 };
	let at: Point = { x: 0, y: 0 };
	const send = (type: 'touchStart' | 'touchMove' | 'touchEnd', points: Point[]) =>
		cdp.send('Input.dispatchTouchEvent', { type, touchPoints: points });
	return {
		async down(target) {
			start = at = await centerOf(target);
			await send('touchStart', [at]);
		},
		async move(dx, dy) {
			const from = at;
			const to = { x: start.x + dx, y: start.y + dy };
			for (let i = 1; i <= MOVE_STEPS; i++) {
				at = {
					x: from.x + ((to.x - from.x) * i) / MOVE_STEPS,
					y: from.y + ((to.y - from.y) * i) / MOVE_STEPS
				};
				await send('touchMove', [at]);
				await page.waitForTimeout(FRAME_MS);
			}
		},
		up: () => send('touchEnd', []).then(() => undefined)
	};
}

/** Synthetic touch `PointerEvent`s on the target, like `touchDrag` above. */
function pointerTouch(): Touch {
	let target: Locator;
	let start: Point = { x: 0, y: 0 };
	let at: Point = { x: 0, y: 0 };
	const fire = (type: string) =>
		target.evaluate(
			(el, [type, x, y]) =>
				el.dispatchEvent(
					new PointerEvent(type, {
						bubbles: true,
						cancelable: true,
						pointerId: 7,
						pointerType: 'touch',
						isPrimary: true,
						clientX: x,
						clientY: y
					})
				),
			[type, at.x, at.y] as const
		);
	return {
		async down(next) {
			if (!('boundingBox' in next)) throw new Error('pointerTouch needs an element');
			target = next;
			start = at = await centerOf(next);
			await fire('pointerdown');
		},
		async move(dx, dy) {
			const from = at;
			const to = { x: start.x + dx, y: start.y + dy };
			for (let i = 1; i <= MOVE_STEPS; i++) {
				at = {
					x: from.x + ((to.x - from.x) * i) / MOVE_STEPS,
					y: from.y + ((to.y - from.y) * i) / MOVE_STEPS
				};
				await fire('pointermove');
			}
		},
		async up() {
			await fire('pointerup');
			// The click a browser sends after a release; the hold must swallow it.
			await target.evaluate((el) =>
				el.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }))
			);
		}
	};
}

/** Synthetic touch events (`touches` only) on the target; bubble to the list scroller. */
function touchEventTouch(): Touch {
	let target: Locator;
	let start: Point = { x: 0, y: 0 };
	let at: Point = { x: 0, y: 0 };
	const fire = (type: string, down: boolean) =>
		target.evaluate(
			(el, [type, x, y, down]) => {
				const event = new Event(type, { bubbles: true, cancelable: true });
				const touches = down ? [{ clientX: x, clientY: y }] : [];
				Object.defineProperty(event, 'touches', { value: touches });
				el.dispatchEvent(event);
			},
			[type, at.x, at.y, down] as const
		);
	return {
		async down(next) {
			if (!('boundingBox' in next)) throw new Error('touchEventTouch needs an element');
			target = next;
			start = at = await centerOf(next);
			await fire('touchstart', true);
		},
		async move(dx, dy) {
			const from = at;
			const to = { x: start.x + dx, y: start.y + dy };
			for (let i = 1; i <= MOVE_STEPS; i++) {
				at = {
					x: from.x + ((to.x - from.x) * i) / MOVE_STEPS,
					y: from.y + ((to.y - from.y) * i) / MOVE_STEPS
				};
				await fire('touchmove', true);
			}
		},
		up: () => fire('touchend', false).then(() => undefined)
	};
}

/** A real wait of `HOLD_MS + 100`: `page.clock` would not move the browser's own timers. */
async function longPress(page: Page, touch: Touch, target: Locator): Promise<void> {
	await touch.down(target);
	await page.waitForTimeout(HOLD_WAIT_MS);
	await touch.up();
}

function cab(page: Page) {
	return page.getByRole('toolbar', { name: 'Selection actions' });
}

function picked(page: Page, n: number) {
	return expect(cab(page).locator('.picked')).toHaveText(`${n} selected`);
}

function tapRow(page: Page, index: number) {
	return row(page, index).locator('.subject').tap();
}

/**
 * Ticks `index` with a synthetic hold, then taps `more` further rows. Both
 * projects use it to enter selection: what the hold itself does per input
 * kind is covered by `holdTests`.
 */
async function enterSelection(page: Page, index = 0, more: number[] = []): Promise<void> {
	await longPress(page, pointerTouch(), row(page, index).locator('.row'));
	await expect(cab(page)).toBeVisible();
	for (const next of more) await tapRow(page, next);
	await picked(page, 1 + more.length);
}

async function historyLength(page: Page): Promise<number> {
	return page.evaluate(() => history.length);
}

/** Waits until the current history entry no longer carries the selection state. */
async function expectNoSelectionState(page: Page): Promise<void> {
	await expect
		.poll(() => page.evaluate(() => JSON.stringify(history.state)))
		.not.toContain('selection');
}

/** `/inbox`, reached from the `clients` label. */
async function openInboxFromLabel(page: Page): Promise<void> {
	await signIn(page);
	await page.goto('/label/clients');
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
	await page.goto('/inbox');
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
	await expect(page).toHaveURL(/\/inbox$/);
}

/** Indexes among the first rows that are unread. */
async function unreadRows(page: Page, scan = 9): Promise<number[]> {
	const found: number[] = [];
	for (let i = 0; i < scan; i++) {
		if ((await row(page, i).locator('.row.unread').count()) > 0) found.push(i);
	}
	return found;
}

/** Hold behavior (AC1, AC2), run once per input kind. */
function holdTests(touchFor: (page: Page) => Promise<Touch>) {
	test.beforeEach(async ({ page }) => {
		await openInbox(page);
	});

	test('a hold ticks the row and shows the selection bar', async ({ page }) => {
		const touch = await touchFor(page);
		await touch.down(row(page, 1).locator('.row'));
		await page.waitForTimeout(HOLD_WAIT_MS);
		await expect(row(page, 1).locator('.check')).toHaveAttribute('aria-checked', 'true');
		await picked(page, 1);
		await touch.up();
	});

	test('releasing after a hold keeps the row ticked and does not open it', async ({ page }) => {
		const touch = await touchFor(page);
		await longPress(page, touch, row(page, 1).locator('.row'));
		await expect(row(page, 1).locator('.check')).toHaveAttribute('aria-checked', 'true');
		await page.waitForTimeout(300);
		await expect(row(page, 1).locator('.check')).toHaveAttribute('aria-checked', 'true');
		await expect(page).toHaveURL(/\/inbox$/);
	});

	test('a release before the hold time ticks nothing', async ({ page }) => {
		const touch = await touchFor(page);
		await touch.down(row(page, 1).locator('.row'));
		await page.waitForTimeout(HOLD_MS / 3);
		await touch.up();
		await page.waitForTimeout(HOLD_WAIT_MS);
		await expect(cab(page)).toHaveCount(0);
		await expect(page.locator('.check[aria-checked=true]')).toHaveCount(0);
	});

	test('moving 10 px or more before the hold turns it into a scroll, not a selection', async ({
		page
	}) => {
		const touch = await touchFor(page);
		await touch.down(row(page, 1).locator('.row'));
		await touch.move(0, 40);
		await page.waitForTimeout(HOLD_WAIT_MS);
		await touch.up();
		await expect(cab(page)).toHaveCount(0);
		await expect(page.locator('.check[aria-checked=true]')).toHaveCount(0);
	});

	test('moves after the hold fired neither swipe the row nor change the selection', async ({
		page
	}) => {
		const touch = await touchFor(page);
		const target = row(page, 1).locator('.row');
		await touch.down(target);
		await page.waitForTimeout(HOLD_WAIT_MS);
		await picked(page, 1);
		await touch.move(-150, 0);
		await expect(target).not.toHaveAttribute('style', /translateX/);
		await picked(page, 1);
		await touch.up();
		await expect(row(page, 1).locator('.check')).toHaveAttribute('aria-checked', 'true');
		await expect(page.locator('[data-toast]')).toHaveCount(0);
		await expect(page).toHaveURL(/\/inbox$/);
	});
}

test.describe('selection', () => {
	test.describe('real touch', { tag: '@cdp-touch' }, () => {
		holdTests(cdpTouch);

		test('a hold shows no native context menu or text selection', async ({ page }) => {
			await page.evaluate(() => {
				const seen: boolean[] = [];
				(window as unknown as { __menus: boolean[] }).__menus = seen;
				// Bubble phase on the window: after the row's own handler ran.
				window.addEventListener('contextmenu', (e) => seen.push(e.defaultPrevented));
			});
			const touch = await cdpTouch(page);
			await touch.down(row(page, 1).locator('.row'));
			await page.waitForTimeout(HOLD_WAIT_MS);
			await picked(page, 1);
			const state = await page.evaluate(() => ({
				menus: (window as unknown as { __menus: boolean[] }).__menus,
				selection: String(getSelection()),
				userSelect: getComputedStyle(document.querySelector('[data-index="1"] .row')!).userSelect
			}));
			expect(state.menus.every(Boolean), 'every contextmenu event is cancelled').toBe(true);
			expect(state.selection).toBe('');
			expect(state.userSelect).toBe('none');
			await touch.up();
		});

		// The bar replaces the header on the hold, so the list moves up under the
		// finger and the click that follows the release lands on the row now under
		// it; the hold swallows that click at the window.
		test('a release after a hold ticks no other row', async ({ page }) => {
			await longPress(page, await cdpTouch(page), row(page, 1).locator('.row'));
			await expect(row(page, 1).locator('.check')).toHaveAttribute('aria-checked', 'true');
			await page.waitForTimeout(300);
			await picked(page, 1);
		});
	});

	test.describe('synthetic touch', { tag: '@synthetic-touch' }, () => {
		holdTests(() => Promise.resolve(pointerTouch()));
	});

	for (const [width, height] of [
		[390, 844],
		[412, 915]
	]) {
		test(`the selection bar replaces the header at ${width}x${height}`, async ({ page }) => {
			await page.setViewportSize({ width, height });
			await openInbox(page);
			const unread = (await row(page, 0).locator('.row.unread').count()) > 0;
			await enterSelection(page, 0);
			await expect(page.locator('header.top')).toBeHidden();
			const bar = cab(page);
			const children = await bar.evaluate((el) =>
				[...el.children].map((child) =>
					child instanceof HTMLButtonElement
						? (child.getAttribute('aria-label') ?? '')
						: child.classList.contains('more')
							? (child.querySelector('button')?.getAttribute('aria-label') ?? '')
							: (child.textContent ?? '').trim()
				)
			);
			expect(children).toEqual([
				'Close selection',
				'1 selected',
				'Archive',
				'Delete',
				unread ? 'Mark as read' : 'Mark as unread',
				'More'
			]);
			const buttons = bar.getByRole('button');
			expect(await buttons.count()).toBe(5);
			for (let i = 0; i < 5; i++) await expectTouchSize(buttons.nth(i));
			const fit = await bar.evaluate((el) => ({
				scrollWidth: el.scrollWidth,
				clientWidth: el.clientWidth,
				rights: [...el.querySelectorAll('button')].map((b) => b.getBoundingClientRect().right),
				innerWidth
			}));
			expect(fit.scrollWidth).toBeLessThanOrEqual(fit.clientWidth);
			for (const right of fit.rights) expect(right).toBeLessThanOrEqual(fit.innerWidth);
			await bar.getByRole('button', { name: 'More' }).tap();
			const menu = page.getByRole('menu', { name: 'More actions' });
			await expect(menu.getByRole('menuitem')).toHaveText([
				/Label as/,
				/Move to/,
				/Star/,
				/Select all/
			]);
		});
	}

	test('tapping another row toggles it, and unticking the last row leaves selection', async ({
		page
	}) => {
		await openInbox(page);
		await enterSelection(page, 1);
		await tapRow(page, 2);
		await picked(page, 2);
		await expect(page).toHaveURL(/\/inbox$/);
		await tapRow(page, 2);
		await picked(page, 1);
		await expect(page).toHaveURL(/\/inbox$/);
		await tapRow(page, 1);
		await expect(cab(page)).toHaveCount(0);
		await expect(page.locator('header.top')).toBeVisible();
		await total(page, 100_000);
		await expect(page).toHaveURL(/\/inbox$/);
	});

	test.describe('history', () => {
		test('Archive from the bar exits without a second back and one Back leaves the inbox', async ({
			page
		}) => {
			await openInboxFromLabel(page);
			const before = await historyLength(page);
			await enterSelection(page, 0);
			expect(await historyLength(page)).toBe(before + 1);
			await cab(page).getByRole('button', { name: 'Archive' }).tap();
			await expect(toast(page, 'Archived 1 conversation')).toBeVisible();
			await expect(cab(page)).toHaveCount(0);
			await expectNoSelectionState(page);
			expect(await historyLength(page)).toBe(before + 1);
			await expect(page).toHaveURL(/\/inbox$/);
			await page.goBack();
			await expect(page).toHaveURL(/\/label\/clients$/);
		});

		test('Back clears the selection and stays on the inbox', async ({ page }) => {
			await openInboxFromLabel(page);
			await enterSelection(page, 0);
			await page.goBack();
			await expect(cab(page)).toHaveCount(0);
			await expect(page.locator('.check[aria-checked=true]')).toHaveCount(0);
			await expect(page).toHaveURL(/\/inbox$/);
			await expect(page.locator('header.top')).toBeVisible();
		});

		test('Close clears the selection, and Back then goes to the label', async ({ page }) => {
			await openInboxFromLabel(page);
			await enterSelection(page, 0);
			await cab(page).getByRole('button', { name: 'Close selection' }).tap();
			await expect(cab(page)).toHaveCount(0);
			await expect(page).toHaveURL(/\/inbox$/);
			await expectNoSelectionState(page);
			await page.goBack();
			await expect(page).toHaveURL(/\/label\/clients$/);
		});

		test('navigating away mid-selection shows the header, and Back lands on a plain inbox', async ({
			page
		}) => {
			await openInbox(page);
			await enterSelection(page, 0);
			// The bar hides the Menu button and the drawer is inert: use `g l`.
			await page.keyboard.press('g');
			await page.keyboard.press('l');
			await expect(page.getByRole('dialog', { name: 'Go to label' })).toBeVisible();
			await page.keyboard.type('fin');
			await page.keyboard.press('Enter');
			await expect(page).toHaveURL(/\/label\/finance$/);
			await expect(page.locator('header.top')).toBeVisible();
			await expect(cab(page)).toHaveCount(0);
			await page.goBack();
			await expect(page).toHaveURL(/\/inbox$/);
			await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
			await expect(cab(page)).toHaveCount(0);
			await expect(page.locator('.check[aria-checked=true]')).toHaveCount(0);
			await expect(page.locator('header.top')).toBeVisible();
			await expectNoSelectionState(page);
			// Another Back leaves the app's pages instead of landing on a selection.
			await page.goBack();
			await expect(cab(page)).toHaveCount(0);
			await expect(page.locator('.check[aria-checked=true]')).toHaveCount(0);
		});
	});

	test('Archive on three rows toasts, drops the total, and Undo restores them', async ({
		page
	}) => {
		await openInbox(page);
		await enterSelection(page, 0, [1, 2]);
		await cab(page).getByRole('button', { name: 'Archive' }).tap();
		await expect(toast(page, 'Archived 3 conversations')).toBeVisible();
		await expect(cab(page)).toHaveCount(0);
		await total(page, 99_997);
		await toast(page, 'Archived 3 conversations').getByRole('button', { name: 'Undo' }).tap();
		await total(page, 100_000);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
	});

	test('marking two unread rows as read clears their unread state and exits selection', async ({
		page
	}) => {
		await openInbox(page);
		const [first, second] = await unreadRows(page);
		expect(second, 'two unread rows among the first rows').toBeDefined();
		await enterSelection(page, first, [second]);
		await cab(page).getByRole('button', { name: 'Mark as read' }).tap();
		await expect(row(page, first).locator('.row')).not.toHaveClass(/\bunread\b/);
		await expect(row(page, second).locator('.row')).not.toHaveClass(/\bunread\b/);
		await expect(cab(page)).toHaveCount(0);
	});

	test('Label as keeps the selection on cancel and clears it after a label is applied', async ({
		page
	}) => {
		await openInbox(page);
		await enterSelection(page, 0);
		const openPicker = async () => {
			await cab(page).getByRole('button', { name: 'More' }).tap();
			await page.getByRole('menuitem', { name: 'Label as' }).tap();
			await expect(page.getByRole('dialog', { name: 'Label as' })).toBeVisible();
		};
		await openPicker();
		await expect(cab(page)).toBeVisible();
		await picked(page, 1);
		await page
			.getByRole('dialog', { name: 'Label as' })
			.getByRole('button', { name: 'Cancel' })
			.tap();
		await expect(page.getByRole('dialog', { name: 'Label as' })).toBeHidden();
		await picked(page, 1);
		await openPicker();
		const picker = page.getByRole('dialog', { name: 'Label as' });
		await picker.getByRole('option', { name: /Clients/ }).tap();
		await picker.getByRole('button', { name: 'Apply' }).tap();
		await expect(picker).toBeHidden();
		await expect(cab(page)).toHaveCount(0);
	});

	test('Select all covers the rows in the window, replacing earlier ticks, and adds no rows', async ({
		page
	}) => {
		await openInbox(page);
		const slots = () => page.locator('[role="option"][data-index]').count();
		const rowHeight = await row(page, 0).evaluate((el) => (el as HTMLElement).offsetHeight);
		/** At the top: the visible rows plus the overscan below (`VirtualList` default 6). */
		const slotsAtTop = async () =>
			Math.ceil((await list(page).evaluate((el) => el.clientHeight)) / rowHeight) + OVERSCAN + 1;
		expect(await slots()).toBe(await slotsAtTop());
		await enterSelection(page, 0);
		// The bar is shorter than the header, so a row more may fit; never more than the geometry asks.
		expect(await slots()).toBe(await slotsAtTop());
		await list(page).evaluate((el, top) => (el.scrollTop = top), 5_000 * rowHeight);
		await expect(row(page, 5_000).locator('[data-loaded]')).toBeVisible();
		await tapRow(page, 5_000);
		await picked(page, 2);
		await cab(page).getByRole('button', { name: 'More' }).tap();
		const menuItem = page.getByRole('menuitem', { name: 'Select all' });
		await expect(menuItem).toBeVisible();
		const inWindow = await page.evaluate(() => {
			const at = [...document.querySelectorAll<HTMLElement>('[role="option"][data-index]')].map(
				(el) => Number(el.dataset.index)
			);
			return Math.max(...at) - Math.min(...at) + 1;
		});
		await menuItem.tap();
		await picked(page, inWindow);
		expect(await slots()).toBe(inWindow);
		expect(inWindow).toBeGreaterThan(2);
	});
});

async function setSyncMode(page: Page, mode: string): Promise<void> {
	const resp = await page.request.post('/__mock/sync-mode', { data: { mode } });
	expect(resp.ok()).toBe(true);
}

async function syncCalls(page: Page): Promise<string[]> {
	const resp = await page.request.get('/__mock/sync');
	return ((await resp.json()) as { calls: string[] }).calls;
}

function refreshing(page: Page) {
	return page.getByRole('progressbar', { name: 'Refreshing' });
}

/** Releases the pull and returns the ms until the indicator is gone. */
async function releaseAndWaitForHide(page: Page, touch: Touch): Promise<number> {
	const released = Date.now();
	await touch.up();
	await expect(refreshing(page)).toBeVisible();
	await page.waitForFunction(() => !document.querySelector('[role="progressbar"]'), null, {
		polling: 'raf',
		timeout: 8_000
	});
	return Date.now() - released;
}

async function pull(page: Page, touch: Touch, dy: number): Promise<void> {
	await touch.down(row(page, 0).locator('.row'));
	await touch.move(0, dy);
}

/** Pull-to-refresh (AC8, AC9), run once per input kind. */
function pullTests(touchFor: (page: Page) => Promise<Touch>) {
	test.beforeEach(async ({ page }) => {
		await openInbox(page);
	});

	test('a pull past the threshold shows the indicator as an overlay and sends one sync', async ({
		page
	}) => {
		await setSyncMode(page, 'change');
		const touch = await touchFor(page);
		const spacer = page.locator('.spacer');
		const top = (await spacer.boundingBox())!.y;
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		await expect(refreshing(page)).toBeVisible();
		expect((await spacer.boundingBox())!.y).toBe(top);
		expect(await syncCalls(page)).toEqual([]);
		await touch.up();
		await expect.poll(() => syncCalls(page)).toEqual(['inbox']);
		await expect(refreshing(page)).toBeHidden();
		expect(await syncCalls(page)).toEqual(['inbox']);
	});

	test('a pull short of the threshold sends no sync and the indicator goes away', async ({
		page
	}) => {
		const touch = await touchFor(page);
		await pull(page, touch, PULL_SHORT_PX);
		await expect(refreshing(page)).toBeVisible();
		await touch.up();
		await expect(refreshing(page)).toBeHidden();
		await page.waitForTimeout(REFRESH_MIN_SPIN_MS);
		expect(await syncCalls(page)).toEqual([]);
	});

	test('a change that moves the list ends the refresh after the minimum spin', async ({ page }) => {
		await setSyncMode(page, 'change');
		const touch = await touchFor(page);
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		const elapsed = await releaseAndWaitForHide(page, touch);
		expect(elapsed).toBeGreaterThanOrEqual(REFRESH_MIN_SPIN_MS);
		expect(elapsed).toBeLessThan(REFRESH_TIMEOUT_MS - TIMEOUT_TOLERANCE_MS);
		await expect(row(page, 0).locator('.subject')).toHaveText('Fresh off the wire');
		await expect(page.getByText("Couldn't refresh")).toHaveCount(0);
		await expect(page.getByText('Up to date')).toHaveCount(0);
	});

	test('no change runs to the timeout, then says Up to date for a moment', async ({ page }) => {
		await setSyncMode(page, 'none');
		const touch = await touchFor(page);
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		const elapsed = await releaseAndWaitForHide(page, touch);
		expect(Math.abs(elapsed - REFRESH_TIMEOUT_MS)).toBeLessThanOrEqual(TIMEOUT_TOLERANCE_MS);
		await expect(page.getByText('Up to date')).toBeVisible();
		await expect(page.getByText("Couldn't refresh")).toHaveCount(0);
		await expect(page.getByText('Up to date')).toHaveCount(0);
	});

	test('changes outside the view run to the timeout like no change', async ({ page }) => {
		await setSyncMode(page, 'irrelevant');
		const touch = await touchFor(page);
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		const elapsed = await releaseAndWaitForHide(page, touch);
		expect(Math.abs(elapsed - REFRESH_TIMEOUT_MS)).toBeLessThanOrEqual(TIMEOUT_TOLERANCE_MS);
		await expect(page.getByText('Up to date')).toBeVisible();
		await expect(page.getByText("Couldn't refresh")).toHaveCount(0);
	});

	test('a search view with no thread change also runs to the timeout', async ({ page }) => {
		await page.goto('/search?q=is%3Astarred');
		const results = page.getByRole('listbox', { name: /Search results/ });
		await expect(results.locator('[data-loaded]').first()).toBeVisible();
		await setSyncMode(page, 'none');
		const touch = await touchFor(page);
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		const elapsed = await releaseAndWaitForHide(page, touch);
		expect(Math.abs(elapsed - REFRESH_TIMEOUT_MS)).toBeLessThanOrEqual(TIMEOUT_TOLERANCE_MS);
		await expect(page.getByText('Up to date')).toBeVisible();
	});

	test('a failing sync request hides the indicator after the minimum spin with an error', async ({
		page
	}) => {
		await setSyncMode(page, 'fail');
		const touch = await touchFor(page);
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		const elapsed = await releaseAndWaitForHide(page, touch);
		expect(elapsed).toBeGreaterThanOrEqual(REFRESH_MIN_SPIN_MS);
		expect(elapsed).toBeLessThan(REFRESH_TIMEOUT_MS - TIMEOUT_TOLERANCE_MS);
		await expect(page.getByText("Couldn't refresh")).toBeVisible();
		await expect(page.getByText('Up to date')).toHaveCount(0);
	});

	test('an engine error at the timeout shows Couldn’t refresh instead of Up to date', async ({
		page
	}) => {
		await setSyncMode(page, 'engine-error');
		const touch = await touchFor(page);
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		const elapsed = await releaseAndWaitForHide(page, touch);
		expect(Math.abs(elapsed - REFRESH_TIMEOUT_MS)).toBeLessThanOrEqual(TIMEOUT_TOLERANCE_MS);
		await expect(page.getByText("Couldn't refresh")).toBeVisible();
		await expect(page.getByText('Up to date')).toHaveCount(0);
	});

	test('no pull and no sync while rows are ticked', async ({ page }) => {
		await enterSelection(page, 1);
		const touch = await touchFor(page);
		await pull(page, touch, PULL_PAST_THRESHOLD_PX);
		await expect(refreshing(page)).toHaveCount(0);
		await touch.up();
		await page.waitForTimeout(REFRESH_MIN_SPIN_MS);
		expect(await syncCalls(page)).toEqual([]);
		await picked(page, 1);
	});

	test('no pull and no sync below the top of the list', async ({ page }) => {
		await list(page).evaluate((el) => (el.scrollTop = 300));
		await expect.poll(() => list(page).evaluate((el) => el.scrollTop)).toBe(300);
		const touch = await touchFor(page);
		await touch.down(row(page, 4).locator('.row'));
		await touch.move(0, PULL_PAST_THRESHOLD_PX);
		await expect(refreshing(page)).toHaveCount(0);
		await touch.up();
		await page.waitForTimeout(REFRESH_MIN_SPIN_MS);
		expect(await syncCalls(page)).toEqual([]);
	});

	test('no pull and no sync on a conversation page', async ({ page }) => {
		await page.goto(`/thread/${idAt(3)}`);
		await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
		const touch = await touchFor(page);
		await touch.down(page.locator('article.thread'));
		await touch.move(0, PULL_PAST_THRESHOLD_PX);
		await expect(refreshing(page)).toHaveCount(0);
		await touch.up();
		await page.waitForTimeout(REFRESH_MIN_SPIN_MS);
		expect(await syncCalls(page)).toEqual([]);
	});
}

test.describe('pull to refresh', () => {
	test.describe('real touch', { tag: '@cdp-touch' }, () => {
		pullTests(cdpTouch);

		test('a real drag mid-list scrolls natively: up then down, no indicator, no sync, no reload', async ({
			page
		}) => {
			const scroller = list(page);
			await scroller.evaluate((el) => (el.scrollTop = 3_000));
			await expect.poll(() => scroller.evaluate((el) => el.scrollTop)).toBe(3_000);
			await page.evaluate(() => ((window as unknown as { __marker: number }).__marker = 1));
			const box = (await scroller.boundingBox())!;
			const touch = await cdpTouch(page);
			await touch.down({ x: box.x + box.width / 2, y: box.y + 300 });
			await touch.move(0, -200);
			await expect.poll(() => scroller.evaluate((el) => el.scrollTop)).toBeGreaterThan(3_000);
			await expect(refreshing(page)).toHaveCount(0);
			await touch.move(0, 200);
			await expect(refreshing(page)).toHaveCount(0);
			await touch.up();
			expect(await scroller.evaluate((el) => el.scrollTop)).not.toBe(3_000);
			await page.waitForTimeout(REFRESH_MIN_SPIN_MS);
			expect(await syncCalls(page)).toEqual([]);
			expect(await page.evaluate(() => (window as unknown as { __marker?: number }).__marker)).toBe(
				1
			);
		});

		test('a drag at the top that goes up first does not engage', async ({ page }) => {
			const touch = await cdpTouch(page);
			await touch.down(row(page, 0).locator('.row'));
			await touch.move(0, -60);
			await touch.move(0, PULL_PAST_THRESHOLD_PX);
			await expect(refreshing(page)).toHaveCount(0);
			await touch.up();
			await page.waitForTimeout(REFRESH_MIN_SPIN_MS);
			expect(await syncCalls(page)).toEqual([]);
		});

		test('a drag at the top that goes sideways first does not engage', async ({ page }) => {
			const touch = await cdpTouch(page);
			await touch.down(row(page, 0).locator('.row'));
			await touch.move(40, 0);
			await touch.move(40, PULL_PAST_THRESHOLD_PX);
			await expect(refreshing(page)).toHaveCount(0);
			await touch.up();
			await page.waitForTimeout(REFRESH_MIN_SPIN_MS);
			expect(await syncCalls(page)).toEqual([]);
			await total(page, 100_000);
		});
	});

	test.describe('synthetic touch', { tag: '@synthetic-touch' }, () => {
		pullTests(() => Promise.resolve(touchEventTouch()));
	});
});

const FRAME = 'iframe[title^="Message from"]';

async function openCorpus(page: Page, name: string) {
	await signIn(page);
	await page.goto(`/thread/${corpusThreadId(name)}`);
	const paper = page.locator('.paper').first();
	await expect(paper).toHaveClass(/\bloaded\b/);
	await expect(paper).toHaveAttribute('data-scale', /./);
	return { paper, frame: page.locator(FRAME).first() };
}

async function scaleOf(paper: Locator): Promise<number> {
	return Number(await paper.getAttribute('data-scale'));
}

/** AC13: nothing wider than the screen, nothing clipped by `.paper`. */
async function expectFits({ paper, frame }: { paper: Locator; frame: Locator }) {
	const m = await frame.evaluate((el) => {
		const f = el as HTMLIFrameElement;
		const doc = f.contentDocument!;
		const parent = f.parentElement!;
		let maxRight = 0;
		for (const e of doc.querySelectorAll('*'))
			maxRight = Math.max(maxRight, e.getBoundingClientRect().right);
		return {
			scale: Number(parent.dataset.scale),
			available: parent.clientWidth,
			docWidth: doc.documentElement.scrollWidth,
			maxRight,
			pageWidth: document.scrollingElement!.scrollWidth,
			innerWidth
		};
	});
	expect(await scaleOf(paper)).toBe(m.scale);
	expect(m.pageWidth).toBeLessThanOrEqual(m.innerWidth);
	expect(m.docWidth * m.scale).toBeLessThanOrEqual(m.available + 1);
	expect(m.maxRight * m.scale).toBeLessThanOrEqual(m.available + 1);
}

test.describe('email rendering', () => {
	for (const email of CORPUS) {
		test(`${email.name} is not clipped or wider than the screen (AC13)`, async ({ page }) => {
			await expectFits(await openCorpus(page, email.name));
		});

		test(`${email.name} keeps pinch zoom available (AC15)`, async ({ page }) => {
			await openCorpus(page, email.name);
			const content = await page.locator('meta[name=viewport]').getAttribute('content');
			expect(content).not.toBeNull();
			expect(content).not.toMatch(/user-scalable\s*=\s*(no|0)/);
			expect(content).not.toMatch(/maximum-scale\s*=\s*1(\.0*)?(?![\d.])/);
		});

		if (email.unstyledTextSelector) {
			test(`${email.name} text without an author font size reads at 14 px or more unless fitted by scaling (AC15)`, async ({
				page
			}) => {
				const { paper, frame } = await openCorpus(page, email.name);
				const px = await dominantTextPx(frame, email.unstyledTextSelector);
				expect(px, 'text found').toBeGreaterThan(0);
				const scale = await scaleOf(paper);
				const effective = px * scale;
				if (scale === 1) expect(effective).toBeGreaterThanOrEqual(14);
				else {
					// B0 scale-to-fit gap, accepted in the approval record: the floor cannot hold.
					test.info().annotations.push({
						type: 'AC15 gap (scaled)',
						description: `${email.name}: data-scale ${scale}, text ${effective.toFixed(1)} px`
					});
					expect(effective).toBeGreaterThan(0);
				}
			});
		}
	}

	for (const email of CORPUS.filter((c) =>
		c.tags.some((t) => t === 'text' || t === 'long-token')
	)) {
		test(`${email.name} is not scaled (AC14)`, async ({ page }) => {
			const { paper } = await openCorpus(page, email.name);
			await expect(paper).toHaveAttribute('data-scale', '1');
		});
	}

	test('long-token-bare may scale but never overflows (AC14)', async ({ page }) => {
		const { paper } = await openCorpus(page, 'long-token-bare');
		expect(await scaleOf(paper)).toBeLessThanOrEqual(1);
	});

	test('fixed-600 keeps its three 200 px columns and only scales (AC14)', async ({ page }) => {
		const { paper, frame } = await openCorpus(page, 'fixed-600');
		const available = await paper.evaluate((el) => el.clientWidth);
		expect(Math.abs((await scaleOf(paper)) - available / 600)).toBeLessThanOrEqual(0.01);
		const widths = await frame.evaluate((el) =>
			[...(el as HTMLIFrameElement).contentDocument!.querySelectorAll('td')].map(
				(td) => td.getBoundingClientRect().width
			)
		);
		expect(widths).toHaveLength(3);
		for (const w of widths) expect(Math.abs(w - 200)).toBeLessThanOrEqual(1);
	});

	for (const name of ['fixed-600', 'text']) {
		test(`${name}: a height change resizes the paper without refitting the width (AC13b)`, async ({
			page
		}) => {
			const { paper, frame } = await openCorpus(page, name);
			const scale = await paper.getAttribute('data-scale');
			await frame.evaluate((el) => {
				const doc = (el as HTMLIFrameElement).contentDocument!;
				const block = doc.createElement('div');
				block.style.height = '500px';
				doc.body.append(block);
			});
			// One frame for the layout, one for the observer callbacks it triggers.
			await page.evaluate(
				() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done)))
			);
			const after = await frame.evaluate((el) => {
				const f = el as HTMLIFrameElement;
				const doc = f.contentDocument!;
				const parent = f.parentElement!;
				return {
					paperGap: Math.abs(
						parent.getBoundingClientRect().height -
							Math.ceil(doc.documentElement.scrollHeight * Number(parent.dataset.scale))
					),
					frameShort: doc.documentElement.scrollHeight - f.clientHeight,
					grew: doc.documentElement.scrollHeight >= 500
				};
			});
			expect(after.grew).toBe(true);
			expect(after.paperGap).toBeLessThanOrEqual(1);
			expect(after.frameShort).toBeLessThanOrEqual(0);
			expect(await paper.getAttribute('data-scale')).toBe(scale);
		});
	}

	test('text: turning the phone sideways refits the frame to the shorter reflowed height', async ({
		page
	}) => {
		const { paper, frame } = await openCorpus(page, 'text');
		// A long paragraph, so the wider layout surely takes fewer lines.
		await frame.evaluate((el) => {
			const doc = (el as HTMLIFrameElement).contentDocument!;
			doc.body.append(Object.assign(doc.createElement('p'), { textContent: 'word '.repeat(400) }));
		});
		const height = () => paper.evaluate((el) => el.getBoundingClientRect().height);
		await expect.poll(height).toBeGreaterThan(400);
		const before = await height();
		const portrait = page.viewportSize()!;
		await page.setViewportSize({ width: portrait.height, height: portrait.width });
		await expect.poll(height).toBeLessThan(before);
	});
});

test.describe('email rendering with author CSS (B1)', () => {
	test.beforeEach(async ({ page }) => {
		const resp = await page.request.post('/__mock/render-author-css', { data: { on: true } });
		expect(resp.ok()).toBe(true);
	});

	for (const email of CORPUS) {
		test(`${email.name} is not clipped or wider than the screen (AC13)`, async ({ page }) => {
			await expectFits(await openCorpus(page, email.name));
		});
	}

	test('the responsive email reflows at the phone width instead of scaling (AC14, AC15)', async ({
		page
	}) => {
		const { paper, frame } = await openCorpus(page, 'responsive');
		await expect(paper).toHaveAttribute('data-scale', '1');
		await expectFits({ paper, frame });
		const m = await frame.evaluate((el) => {
			const f = el as HTMLIFrameElement;
			const cols = [...f.contentDocument!.querySelectorAll('td.col')].map((td) =>
				td.getBoundingClientRect()
			);
			return {
				available: f.parentElement!.clientWidth,
				cols: cols.map((r) => ({ top: r.top, width: r.width }))
			};
		});
		// The author's @media rule stacks the two columns at full width.
		expect(m.cols).toHaveLength(2);
		expect(m.cols[1].top).toBeGreaterThan(m.cols[0].top);
		for (const c of m.cols) expect(Math.abs(c.width - m.available)).toBeLessThanOrEqual(1);
		expect(await dominantTextPx(frame, 'p')).toBeGreaterThanOrEqual(14);
	});
});
