import type { Locator, Page, Request, Route } from '@playwright/test';
import { THREAD_COUNT, idAt, rowAt } from './mock/fixture.ts';
import { expect, list, openInbox, row, signIn, test } from './helpers.ts';

const DESKTOP = { width: 1280, height: 800 };
/** VirtualList's default overscan. */
const OVERSCAN = 6;

test.use({ viewport: DESKTOP });

interface MockOp {
	opId: string;
	kind: string;
	status: string;
	threadIds: number[];
}

async function mockOps(page: Page): Promise<MockOp[]> {
	return ((await (await page.request.get('/__mock/state')).json()) as { ops: MockOp[] }).ops;
}

/** Fixture positions of unread and read threads, in date order. */
const UNREAD: number[] = [];
const READ: number[] = [];
for (let i = 0; i < THREAD_COUNT; i++) (rowAt(i).unreadCount > 0 ? UNREAD : READ).push(i);

/**
 * Stores prefs before the app's first load only; reloads keep whatever the
 * app wrote since (an init script runs again on every navigation).
 */
async function seed(page: Page, values: Record<string, string>): Promise<void> {
	await page.addInitScript((v) => {
		if (sessionStorage.getItem('e2e.seeded')) return;
		sessionStorage.setItem('e2e.seeded', '1');
		for (const [k, value] of Object.entries(v)) localStorage.setItem(k, value);
	}, values);
}

async function stored(page: Page): Promise<Record<string, string>> {
	return page.evaluate(() => {
		const out: Record<string, string> = {};
		for (let i = 0; i < localStorage.length; i++) {
			const k = localStorage.key(i)!;
			if (k.startsWith('tagmail.') && k !== 'tagmail.listPath') out[k] = localStorage.getItem(k)!;
		}
		return out;
	});
}

function thread(page: Page, id: number): Locator {
	return page.locator(`[role="option"][id="thread-${id}"]`);
}

function toast(page: Page, text: string | RegExp) {
	return page.locator('[data-toast]').filter({ hasText: text });
}

function total(page: Page, n: number) {
	return expect(page.locator('.total')).toHaveText(
		`${n.toLocaleString('en-US')} conversation${n === 1 ? '' : 's'}`
	);
}

function gear(page: Page) {
	return page.getByRole('button', { name: 'Quick settings' });
}

function panel(page: Page) {
	return page.getByRole('group', { name: 'Quick settings' });
}

function separator(page: Page) {
	return page.getByRole('separator', { name: 'Resize navigation' });
}

async function openFlyout(page: Page): Promise<void> {
	await gear(page).click();
	await expect(panel(page)).toBeVisible();
}

async function choose(page: Page, group: string, option: string): Promise<void> {
	await panel(page)
		.getByRole('group', { name: group })
		.getByRole('radio', { name: option, exact: true })
		.check();
}

/** The section header an option is described by: "Unread" or "Everything else". */
async function sectionOf(option: Locator): Promise<string> {
	const id = await option.getAttribute('aria-describedby');
	expect(id, 'option has aria-describedby').toBeTruthy();
	return (await option.page().locator(`[id="${id}"]`).textContent())!.trim();
}

/** Makes the page look hidden, as when the player switches tabs. */
async function hideTab(page: Page): Promise<void> {
	await page.evaluate(() => {
		Object.defineProperty(document, 'visibilityState', { value: 'hidden', configurable: true });
		document.dispatchEvent(new Event('visibilitychange'));
		Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true });
		document.dispatchEvent(new Event('visibilitychange'));
	});
}

function isUnreadRefresh(r: Request): boolean {
	return r.method() === 'GET' && decodeURIComponent(r.url()).includes('is:unread/index');
}

async function scrollerTop(page: Page, name = 'Inbox'): Promise<number> {
	return list(page, name).evaluate((el) => el.scrollTop);
}

test.describe('resizable navigation', () => {
	test('dragging resizes the nav live, clamped to 200-400', async ({ page }) => {
		await openInbox(page);
		const side = page.locator('.side');
		const handle = separator(page);
		await expect(handle).toHaveAttribute('aria-orientation', 'vertical');
		await expect(handle).toHaveAttribute('aria-valuenow', '236');
		const box = (await handle.boundingBox())!;
		const x = box.x + box.width / 2;
		const y = box.y + box.height / 2;
		// The handle sits above main's layers and receives the pointer.
		expect(
			await page.evaluate(
				([px, py]) => {
					const el = document.elementFromPoint(px, py);
					return el?.getAttribute('role');
				},
				[x, y]
			)
		).toBe('separator');

		await page.mouse.move(x, y);
		await page.mouse.down();
		await page.mouse.move(100, y, { steps: 4 });
		await expect.poll(async () => (await side.boundingBox())!.width).toBe(200);
		await page.mouse.move(600, y, { steps: 6 });
		await expect.poll(async () => (await side.boundingBox())!.width).toBe(400);
		// Not stored until release.
		expect((await stored(page))['tagmail.navWidth']).toBeUndefined();
		await page.mouse.up();
		await expect(handle).toHaveAttribute('aria-valuenow', '400');
		expect((await stored(page))['tagmail.navWidth']).toBe('400');
		const mainBox = (await page.locator('main').boundingBox())!;
		expect(mainBox.x).toBeGreaterThanOrEqual(400);
		const sideBox = (await side.boundingBox())!;
		expect(sideBox.x).toBe(0);
	});

	test('arrow keys step by 8 without firing list shortcuts; double-click resets', async ({
		page
	}) => {
		await openInbox(page);
		const handle = separator(page);
		await handle.focus();
		await page.keyboard.press('ArrowRight');
		await expect(handle).toHaveAttribute('aria-valuenow', '244');
		await page.keyboard.press('ArrowRight');
		await expect(handle).toHaveAttribute('aria-valuenow', '252');
		await page.keyboard.press('ArrowLeft');
		await expect(handle).toHaveAttribute('aria-valuenow', '244');
		await expect(handle).toHaveAttribute('aria-valuetext', '244 pixels');
		await expect(handle).toBeFocused();
		expect((await page.locator('.side').boundingBox())!.width).toBe(244);
		// The list's selection did not move with the arrows.
		await expect(row(page, 0)).toHaveAttribute('aria-selected', 'true');
		await expect(row(page, 1)).toHaveAttribute('aria-selected', 'false');
		await expect(page).toHaveURL(/\/inbox$/);

		await handle.dblclick();
		await expect(handle).toHaveAttribute('aria-valuenow', '236');
		expect((await stored(page))['tagmail.navWidth']).toBeUndefined();
	});

	test('the width survives a reload and the brand follows it', async ({ page }) => {
		await openInbox(page);
		const handle = separator(page);
		const box = (await handle.boundingBox())!;
		const y = box.y + box.height / 2;
		await page.mouse.move(box.x + box.width / 2, y);
		await page.mouse.down();
		await page.mouse.move(box.x + box.width / 2 + 64, y, { steps: 4 });
		await page.mouse.up();
		await expect(handle).toHaveAttribute('aria-valuenow', '300');
		await page.reload();
		await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
		await expect(handle).toHaveAttribute('aria-valuenow', '300');
		expect((await page.locator('.side').boundingBox())!.width).toBe(300);
		const brand = await page
			.locator('.brand')
			.evaluate((el) => parseFloat(getComputedStyle(el).width));
		expect(brand).toBe(300 - 28);

		await page.setViewportSize({ width: 700, height: 800 });
		await expect(handle).toBeHidden();
	});
});

test.describe('quick settings flyout', () => {
	test('the gear is the right-most top-bar item; Escape and outside clicks close it', async ({
		page
	}) => {
		await openInbox(page);
		const g = gear(page);
		const right = await page
			.locator('header.top > *')
			.evaluateAll((els) =>
				els
					.filter((e) => e.getBoundingClientRect().width > 0)
					.map((e) => e.getBoundingClientRect().right)
			);
		expect((await g.boundingBox())!.x + (await g.boundingBox())!.width).toBe(Math.max(...right));
		await expect(g).toHaveAttribute('aria-expanded', 'false');

		await openFlyout(page);
		await expect(g).toHaveAttribute('aria-expanded', 'true');
		await g.focus();
		await page.keyboard.press('Escape');
		await expect(panel(page)).toBeHidden();
		await expect(g).toBeFocused();

		await openFlyout(page);
		const radio = panel(page).getByRole('radio', { name: 'Comfortable' });
		await radio.focus();
		await page.keyboard.press('Escape');
		await expect(panel(page)).toBeHidden();
		await expect(g).toBeFocused();

		await openFlyout(page);
		await page.locator('.brand').click();
		await expect(panel(page)).toBeHidden();
		await expect(g).toHaveAttribute('aria-expanded', 'false');
	});

	test('it holds four radio groups and a link to all settings', async ({ page }) => {
		await openInbox(page);
		await openFlyout(page);
		const groups = panel(page).getByRole('group');
		await expect(groups).toHaveCount(4);
		for (const [name, options] of [
			['Density', ['Default', 'Comfortable', 'Compact']],
			['Theme', ['Light', 'Dark', 'System']],
			['Inbox type', ['Default', 'Unread first']],
			['Reading pane', ['No split', 'Right of list', 'Below list']]
		] as const) {
			const group = panel(page).getByRole('group', { name });
			await expect(group.getByRole('radio')).toHaveCount(options.length);
			for (const o of options) {
				await expect(group.getByRole('radio', { name: o, exact: true })).toBeVisible();
			}
		}
		await expect(
			panel(page).getByRole('group', { name: 'Density' }).getByRole('radio', { name: 'Default' })
		).toBeChecked();
		await panel(page).getByRole('link', { name: 'See all settings' }).click();
		await expect(page).toHaveURL(/\/settings\/accounts$/);
		await expect(panel(page)).toBeHidden();
	});

	test('Enter, e, # and Shift+I do nothing while the flyout is open', async ({ page }) => {
		await openInbox(page);
		await list(page).focus();
		await openFlyout(page);
		await panel(page).getByRole('radio', { name: 'Comfortable' }).focus();
		for (const key of ['Enter', 'e', '#', 'Shift+I', 'j']) await page.keyboard.press(key);
		await expect(panel(page)).toBeVisible();
		await expect(page).toHaveURL(/\/inbox$/);
		await total(page, THREAD_COUNT);
		await expect(row(page, 0)).toHaveAttribute('aria-selected', 'true');
		await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
		await expect(page.locator('[data-toast]')).toHaveCount(0);
		expect(await mockOps(page)).toEqual([]);
		// Density stayed on Default: the radio only got focus.
		await expect(panel(page).getByRole('radio', { name: 'Comfortable' })).not.toBeChecked();
	});

	test('choices apply at once, persist across a reload, and defaults remove their keys', async ({
		page
	}) => {
		await openInbox(page);
		await openFlyout(page);
		await choose(page, 'Density', 'Comfortable');
		await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable');
		await choose(page, 'Theme', 'Dark');
		await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
		await choose(page, 'Inbox type', 'Unread first');
		await expect(row(page, 0)).toHaveId(`thread-${idAt(UNREAD[0])}`);
		await choose(page, 'Reading pane', 'Right of list');
		await expect(page.locator('.split-host')).toHaveCount(1);
		// Focus stays in the flyout through every remount.
		expect(
			await page.evaluate(() => !!document.activeElement?.closest('[data-quick-settings]'))
		).toBe(true);
		await expect(panel(page)).toBeVisible();
		expect(await stored(page)).toEqual({
			'tagmail.density': 'comfortable',
			'tagmail.theme': 'dark',
			'tagmail.inboxType': 'unread',
			'tagmail.readingPane': 'right'
		});

		await page.reload();
		await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
		await expect(page.locator('html')).toHaveAttribute('data-density', 'comfortable');
		await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
		await expect(page.locator('.split-host')).toHaveCount(1);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(UNREAD[0])}`);
		await openFlyout(page);
		await expect(
			panel(page)
				.getByRole('group', { name: 'Density' })
				.getByRole('radio', { name: 'Comfortable' })
		).toBeChecked();
		await expect(panel(page).getByRole('radio', { name: 'Unread first' })).toBeChecked();
		await expect(panel(page).getByRole('radio', { name: 'Right of list' })).toBeChecked();

		await choose(page, 'Density', 'Default');
		await choose(page, 'Theme', 'System');
		await choose(page, 'Inbox type', 'Default');
		await choose(page, 'Reading pane', 'No split');
		expect(await stored(page)).toEqual({});
		await expect(page.locator('.split-host')).toHaveCount(0);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
		await expect(page.locator('html')).not.toHaveAttribute('data-theme', /./);
	});

	test('invalid stored values fall back to the defaults', async ({ page }) => {
		await seed(page, {
			'tagmail.density': 'huge',
			'tagmail.inboxType': 'sideways',
			'tagmail.readingPane': 'left',
			'tagmail.navWidth': 'wide'
		});
		await openInbox(page);
		expect((await row(page, 0).boundingBox())!.height).toBe(40);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
		await expect(page.locator('.split-host')).toHaveCount(0);
		await expect(separator(page)).toHaveAttribute('aria-valuenow', '236');
	});
});

test.describe('density', () => {
	test('rows are 40, 48 and 32 px tall and the top row stays put', async ({ page }) => {
		await openInbox(page);
		expect((await row(page, 0).boundingBox())!.height).toBe(40);
		await list(page).evaluate((el) => (el.scrollTop = 100 * 40));
		await expect(row(page, 100)).toBeVisible();
		await openFlyout(page);

		await choose(page, 'Density', 'Comfortable');
		await expect.poll(async () => (await row(page, 100).boundingBox())?.height).toBe(48);
		expect(await scrollerTop(page)).toBe(100 * 48);

		await choose(page, 'Density', 'Compact');
		await expect.poll(async () => (await row(page, 100).boundingBox())?.height).toBe(32);
		expect(await scrollerTop(page)).toBe(100 * 32);

		await choose(page, 'Density', 'Default');
		await expect.poll(async () => (await row(page, 100).boundingBox())?.height).toBe(40);
		expect(await scrollerTop(page)).toBe(100 * 40);
	});
});

test.describe('unread first', () => {
	// WebKit does not route requests that go through the service worker, so holding the ops POST
	// with page.route only works when the worker is out of the way.
	test.use({ serviceWorkers: 'block' });

	test.beforeEach(async ({ page }) => {
		await seed(page, { 'tagmail.inboxType': 'unread' });
	});

	test('the inbox shows Unread, then Everything else, each newest first', async ({ page }) => {
		await openInbox(page);
		const box = list(page);
		const headers = box.locator('.slot.header');
		await expect(headers.first()).toHaveText('Unread');
		await expect(headers.first()).toHaveAttribute('aria-hidden', 'true');
		await expect(headers.first()).not.toHaveAttribute('data-index', /./);
		for (let i = 0; i < 5; i++) {
			await expect(row(page, i)).toHaveId(`thread-${idAt(UNREAD[i])}`);
			expect(await sectionOf(row(page, i))).toBe('Unread');
		}
		await total(page, THREAD_COUNT);

		const split = UNREAD.length;
		// The header slot sits between the two sections: rows 0..split-1, header, rows split..
		await box.evaluate((el, top) => (el.scrollTop = top), (split - 3) * 40);
		await expect(headers.filter({ hasText: 'Everything else' })).toBeVisible();
		await expect(row(page, split - 1)).toHaveId(`thread-${idAt(UNREAD[split - 1])}`);
		expect(await sectionOf(row(page, split - 1))).toBe('Unread');
		for (let i = 0; i < 4; i++) {
			await expect(row(page, split + i)).toHaveId(`thread-${idAt(READ[i])}`);
			expect(await sectionOf(row(page, split + i))).toBe('Everything else');
		}
		const header = (await headers.filter({ hasText: 'Everything else' }).boundingBox())!;
		const last = (await row(page, split - 1).boundingBox())!;
		const first = (await row(page, split).boundingBox())!;
		expect(header.y).toBeGreaterThanOrEqual(last.y + last.height - 0.5);
		expect(first.y).toBeGreaterThanOrEqual(header.y + header.height - 0.5);
		// aria-setsize counts threads, not header slots.
		await expect(row(page, split)).toHaveAttribute('aria-setsize', String(THREAD_COUNT));
		await expect(row(page, split)).toHaveAttribute('aria-posinset', String(split + 1));

		// The rendered-slot bound holds.
		const { slots, height } = await box.evaluate((el) => ({
			slots: el.querySelectorAll('.spacer > .slot').length,
			height: el.clientHeight
		}));
		expect(slots).toBeLessThanOrEqual(Math.ceil(height / 40) + 2 * OVERSCAN + 2);
	});

	test('Default restores the flat list', async ({ page }) => {
		await openInbox(page);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(UNREAD[0])}`);
		await openFlyout(page);
		await choose(page, 'Inbox type', 'Default');
		await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
		await expect(list(page).locator('.slot.header')).toHaveCount(0);
		await expect(row(page, 0)).not.toHaveAttribute('aria-describedby', /./);
	});

	test('labels, mailboxes and search stay flat', async ({ page }) => {
		await signIn(page);
		for (const [path, name] of [
			['/label/finance', 'Finance'],
			['/mailbox/1', 'INBOX'],
			['/search?q=has%3Aattachment', 'Search results for has:attachment']
		] as const) {
			await page.goto(path);
			const box = page.getByRole('listbox', { name });
			await expect(box.locator('[data-loaded]').first()).toBeVisible();
			await expect(box.locator('.slot.header')).toHaveCount(0);
			await expect(box.locator('[aria-describedby]')).toHaveCount(0);
		}
	});

	test('every option is described by an existing section header', async ({ page }) => {
		await openInbox(page);
		const ids = await list(page)
			.locator('[role="option"]')
			.evaluateAll((els) => els.map((e) => e.getAttribute('aria-describedby')));
		expect(ids.length).toBeGreaterThan(5);
		for (const id of ids) {
			expect(id).toBeTruthy();
			const target = page.locator(`[id="${id}"]`);
			await expect(target).toHaveCount(1);
			await expect(target).toHaveText(/^(Unread|Everything else)$/);
		}
	});

	test('a row marked read stays put until the tab is hidden, then moves', async ({ page }) => {
		await openInbox(page);
		await list(page).focus();
		const id = idAt(UNREAD[0]);
		const target = thread(page, id);
		await expect(target.locator('.row')).toHaveClass(/\bunread\b/);

		// Hold the op on its way to the server.
		let release!: () => void;
		const held = new Promise<void>((resolve) => (release = resolve));
		let posted = false;
		await page.route(
			(url) => url.pathname === '/api/ops',
			async (route: Route) => {
				if (route.request().method() !== 'POST') return route.continue();
				posted = true;
				await held;
				await route.continue();
			}
		);
		await page.keyboard.press('Shift+I');
		await expect.poll(() => posted).toBe(true);
		// Before the server answers: read, and still first in Unread.
		await expect(target.locator('.row')).not.toHaveClass(/\bunread\b/);
		await expect(target).toHaveAttribute('data-index', '0');
		expect(await sectionOf(target)).toBe('Unread');

		const refreshed = page.waitForRequest(isUnreadRefresh);
		release();
		await expect
			.poll(async () => (await mockOps(page)).find((o) => o.kind === 'markRead')?.status)
			.toBe('done');
		// After the ack and the unread refresh that follows it: no bounce, still held.
		await (await refreshed).response();
		await expect(target.locator('.row')).not.toHaveClass(/\bunread\b/);
		await expect(target).toHaveAttribute('data-index', '0');
		expect(await sectionOf(target)).toBe('Unread');
		await expect(row(page, 1)).toHaveId(`thread-${idAt(UNREAD[1])}`);

		// Leaving the tab lets it move to where it belongs.
		await hideTab(page);
		// Unread lost one row; Everything else starts with the newer read idAt(0).
		await expect(target).toHaveAttribute('data-index', String(UNREAD.length));
		expect(await sectionOf(target)).toBe('Everything else');
		await list(page).evaluate((el) => (el.scrollTop = 0));
		await expect(row(page, 0)).toHaveId(`thread-${idAt(UNREAD[1])}`);
		await total(page, THREAD_COUNT);
	});

	test('a window blur releases held rows as well', async ({ page }) => {
		await openInbox(page);
		await list(page).focus();
		const target = thread(page, idAt(UNREAD[0]));
		await page.keyboard.press('Shift+I');
		await expect(target.locator('.row')).not.toHaveClass(/\bunread\b/);
		await expect(target).toHaveAttribute('data-index', '0');
		await page.evaluate(() => {
			document.hasFocus = () => false;
			window.dispatchEvent(new Event('blur'));
		});
		// Unread lost one row; Everything else starts with the newer read idAt(0).
		await expect(target).toHaveAttribute('data-index', String(UNREAD.length));
		expect(await sectionOf(target)).toBe('Everything else');
	});

	test('archive leaves at once and Undo restores the row into its section', async ({ page }) => {
		await openInbox(page);
		await list(page).focus();
		await page.keyboard.press('e');
		await expect(toast(page, 'Archived 1 conversation')).toBeVisible();
		await total(page, THREAD_COUNT - 1);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(UNREAD[1])}`);
		await toast(page, 'Archived 1 conversation').getByRole('button', { name: 'Undo' }).click();
		await total(page, THREAD_COUNT);
		await expect(row(page, 0)).toHaveId(`thread-${idAt(UNREAD[0])}`);
		expect(await sectionOf(row(page, 0))).toBe('Unread');

		// The oldest thread is read, so it is the last row of Everything else.
		await list(page).focus();
		await page.keyboard.press('End');
		const last = THREAD_COUNT - 1;
		await expect(row(page, last)).toHaveAttribute('aria-selected', 'true');
		const lastId = idAt(READ[READ.length - 1]);
		await expect(row(page, last)).toHaveId(`thread-${lastId}`);
		await page.keyboard.press('e');
		await expect(toast(page, 'Archived 1 conversation')).toBeVisible();
		await expect(thread(page, lastId)).toHaveCount(0);
		await toast(page, 'Archived 1 conversation').getByRole('button', { name: 'Undo' }).click();
		await expect(thread(page, lastId)).toHaveAttribute('data-index', String(last));
		expect(await sectionOf(thread(page, lastId))).toBe('Everything else');
	});

	test('new mail lands in Unread', async ({ page }) => {
		await openInbox(page);
		await expect(page.locator('.live.on')).toBeVisible();
		await page.request.post('/__mock/new-mail', { data: { subject: 'Fresh unread thing' } });
		await expect(row(page, 0).locator('.subject')).toHaveText('Fresh unread thing');
		expect(await sectionOf(row(page, 0))).toBe('Unread');
		await total(page, THREAD_COUNT + 1);
	});
});

test.describe('reading pane', () => {
	test('No split renders no split wrapper', async ({ page }) => {
		await openInbox(page);
		await expect(page.locator('.split-host, .split, .list-pane')).toHaveCount(0);
		await row(page, 0).click();
		await expect(page).toHaveURL(new RegExp(`/thread/${idAt(0)}$`));
		await expect(list(page)).toHaveCount(0);
	});

	for (const pane of ['right', 'below'] as const) {
		test(`${pane}: rows open next to the list, which keeps its scroll`, async ({ page }) => {
			await seed(page, { 'tagmail.readingPane': pane });
			await openInbox(page);
			await expect(page.getByText('Select a conversation')).toBeVisible();
			const listBox = (await page.locator('.list-pane').boundingBox())!;
			const paneBox = (await page.locator('.reading-pane').boundingBox())!;
			if (pane === 'right') expect(paneBox.x).toBeGreaterThanOrEqual(listBox.x + listBox.width - 1);
			else expect(paneBox.y).toBeGreaterThanOrEqual(listBox.y + listBox.height - 1);

			await list(page).evaluate((el) => {
				el.scrollTop = 50 * 40;
				(el as unknown as { e2eMark: boolean }).e2eMark = true;
			});
			await row(page, 52).click();
			await expect(page).toHaveURL(new RegExp(`/thread/${idAt(52)}$`));
			await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(52).subject);
			await row(page, 54).click();
			await expect(page).toHaveURL(new RegExp(`/thread/${idAt(54)}$`));
			await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(54).subject);
			await expect(row(page, 54)).toHaveAttribute('aria-selected', 'true');
			expect(await scrollerTop(page)).toBe(50 * 40);
			// The same listbox element: the list was never remounted.
			expect(
				await list(page).evaluate((el) => (el as unknown as { e2eMark?: boolean }).e2eMark)
			).toBe(true);
			await expect(page.getByText('Select a conversation')).toHaveCount(0);
		});
	}

	test('ArrowDown and Enter open the next thread with focus kept in the list; u returns it', async ({
		page
	}) => {
		await seed(page, { 'tagmail.readingPane': 'right' });
		await openInbox(page);
		await row(page, 0).click();
		await expect(page).toHaveURL(new RegExp(`/thread/${idAt(0)}$`));
		await expect(list(page)).toBeFocused();
		await page.keyboard.press('ArrowDown');
		await expect(row(page, 1)).toHaveAttribute('aria-selected', 'true');
		await page.keyboard.press('Enter');
		await expect(page).toHaveURL(new RegExp(`/thread/${idAt(1)}$`));
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(1).subject);
		await expect(list(page)).toBeFocused();

		// From the reading pane, u closes the conversation and hands focus to the list.
		await page.getByRole('heading', { level: 1 }).focus();
		await page.keyboard.press('u');
		await expect(page).toHaveURL(/\/inbox$/);
		await expect(page.getByText('Select a conversation')).toBeVisible();
		await expect(list(page)).toBeFocused();
		await expect(row(page, 1)).toHaveAttribute('aria-selected', 'true');
	});

	test('a reload on /thread/<id> restores the split, and j moves on', async ({ page }) => {
		await seed(page, { 'tagmail.readingPane': 'right' });
		await openInbox(page);
		await row(page, 10).click();
		await expect(page).toHaveURL(new RegExp(`/thread/${idAt(10)}$`));
		await page.reload();
		await expect(page.locator('.split-host')).toHaveCount(1);
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(10).subject);
		await expect(row(page, 10)).toHaveAttribute('aria-selected', 'true');
		await page.keyboard.press('j');
		await expect(page).toHaveURL(new RegExp(`/thread/${idAt(11)}$`));
		await expect(row(page, 11)).toHaveAttribute('aria-selected', 'true');
		await page.goBack();
		await expect(page).toHaveURL(new RegExp(`/thread/${idAt(10)}$`));
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(10).subject);
	});

	for (const start of ['warm', 'cold'] as const) {
		test(`${start}: Enter, x and e go to the list or the open thread by focus`, async ({
			page
		}) => {
			await seed(page, { 'tagmail.readingPane': 'right' });
			const open = idAt(READ[1]); // position 8
			if (start === 'warm') {
				await openInbox(page);
				await row(page, READ[1]).click();
			} else {
				await signIn(page);
				await page.goto(`/thread/${open}`);
				await expect(row(page, READ[1])).toHaveAttribute('aria-selected', 'true');
				await list(page).focus();
			}
			await expect(page).toHaveURL(new RegExp(`/thread/${open}$`));
			await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(READ[1]).subject);
			await expect(list(page)).toBeFocused();

			// Enter on the open thread's own highlight does nothing new.
			await page.keyboard.press('Enter');
			await expect(page).toHaveURL(new RegExp(`/thread/${open}$`));

			// x ticks the highlighted row; e archives it while the open thread stays.
			await page.keyboard.press('ArrowDown');
			await expect(row(page, READ[1] + 1)).toHaveAttribute('aria-selected', 'true');
			await page.keyboard.press('x');
			await expect(row(page, READ[1] + 1).getByRole('checkbox')).toHaveAttribute(
				'aria-checked',
				'true'
			);
			await page.keyboard.press('x');
			await expect(row(page, READ[1] + 1).getByRole('checkbox')).toHaveAttribute(
				'aria-checked',
				'false'
			);
			const highlighted = idAt(READ[1] + 1);
			await page.keyboard.press('e');
			await expect(toast(page, 'Archived 1 conversation')).toBeVisible();
			await expect(thread(page, highlighted)).toHaveCount(0);
			await expect(page).toHaveURL(new RegExp(`/thread/${open}$`));
			await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(READ[1]).subject);
			await total(page, THREAD_COUNT - 1);

			// Enter opens a highlight other than the open thread.
			await list(page).focus();
			const sel = list(page).locator('[role="option"][aria-selected="true"]');
			let selId = Number((await sel.getAttribute('id'))!.slice('thread-'.length));
			if (selId === open) {
				await page.keyboard.press('ArrowDown');
				await expect(sel).not.toHaveId(`thread-${open}`);
				selId = Number((await sel.getAttribute('id'))!.slice('thread-'.length));
			}
			await page.keyboard.press('Enter');
			await expect(page).toHaveURL(new RegExp(`/thread/${selId}$`));
			await expect(list(page)).toBeFocused();

			// With focus in the pane, e archives the open thread and the pane empties.
			const heading = page.getByRole('heading', { level: 1 });
			await expect(heading).toHaveText(
				(await thread(page, selId).locator('.subject').textContent())!
			);
			await heading.focus();
			await page.keyboard.press('e');
			await expect(page).toHaveURL(/\/inbox$/);
			await expect(page.getByText('Select a conversation')).toBeVisible();
			await expect(thread(page, selId)).toHaveCount(0);
			await total(page, THREAD_COUNT - 2);
			await expect(list(page)).toBeFocused();
		});
	}

	test('a thread left open for 3 s in the pane is marked read', async ({ page }) => {
		await seed(page, { 'tagmail.readingPane': 'right' });
		await openInbox(page);
		const id = idAt(UNREAD[0]);
		const opPosted = page.waitForRequest(
			(r) => r.method() === 'POST' && new URL(r.url()).pathname === '/api/ops'
		);
		await row(page, UNREAD[0]).click();
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(UNREAD[0]).subject);
		const shown = Date.now();
		await expect(thread(page, id).locator('.row')).toHaveClass(/\bunread\b/);

		await page.waitForTimeout(2000);
		await expect(thread(page, id).locator('.row')).toHaveClass(/\bunread\b/);
		expect(await mockOps(page)).toEqual([]);

		const op = await opPosted;
		const elapsed = Date.now() - shown;
		expect(elapsed).toBeGreaterThan(2000);
		expect(elapsed).toBeLessThan(3500);
		expect(op.postDataJSON()).toMatchObject({ kind: 'markRead', threadIds: [id] });
		await expect(thread(page, id).locator('.row')).not.toHaveClass(/\bunread\b/);
		// Held in Unread like any other read.
		await expect(thread(page, id)).toHaveAttribute('data-index', String(UNREAD[0]));
	});

	test('leaving within 3 s cancels the auto-read', async ({ page }) => {
		await seed(page, { 'tagmail.readingPane': 'right' });
		await openInbox(page);
		const id = idAt(UNREAD[0]);
		await row(page, UNREAD[0]).click();
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(UNREAD[0]).subject);
		await page.waitForTimeout(1000);
		await list(page).focus();
		await page.keyboard.press('u');
		await expect(page).toHaveURL(/\/inbox$/);
		await page.waitForTimeout(3000);
		expect(await mockOps(page)).toEqual([]);
		await expect(thread(page, id).locator('.row')).toHaveClass(/\bunread\b/);
	});

	test('without a split, opening a thread does not mark it read', async ({ page }) => {
		await openInbox(page);
		await row(page, UNREAD[0]).click();
		await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(UNREAD[0]).subject);
		await page.waitForTimeout(3500);
		expect(await mockOps(page)).toEqual([]);
	});

	test('a narrow container stacks the Right pane', async ({ page }) => {
		await page.setViewportSize({ width: 1024, height: 800 });
		await seed(page, { 'tagmail.readingPane': 'right', 'tagmail.navWidth': '400' });
		await openInbox(page);
		await row(page, 0).click();
		await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
		const listBox = (await page.locator('.list-pane').boundingBox())!;
		const paneBox = (await page.locator('.reading-pane').boundingBox())!;
		expect(paneBox.y).toBeGreaterThanOrEqual(listBox.y + listBox.height - 1);
		const { scrollWidth, innerWidth } = await page.evaluate(() => ({
			scrollWidth: document.documentElement.scrollWidth,
			innerWidth: window.innerWidth
		}));
		expect(scrollWidth).toBeLessThanOrEqual(innerWidth);
	});

	test('phone width never renders the split and opens threads full screen', async ({ page }) => {
		await page.setViewportSize({ width: 390, height: 844 });
		await seed(page, { 'tagmail.readingPane': 'right' });
		await page.addInitScript(() => {
			const w = window as unknown as { e2eSplitSeen: string[] };
			w.e2eSplitSeen = [];
			const SELECTOR = '.split, .list-pane, .split-host';
			new MutationObserver((records) => {
				for (const r of records) {
					for (const n of r.addedNodes) {
						if (!(n instanceof Element)) continue;
						if (n.matches(SELECTOR) || n.querySelector(SELECTOR)) w.e2eSplitSeen.push(n.className);
					}
				}
			}).observe(document, { childList: true, subtree: true });
		});
		await openInbox(page);
		await row(page, 0).click();
		await expect(page).toHaveURL(new RegExp(`/thread/${idAt(0)}$`));
		await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
		await expect(list(page)).toHaveCount(0);
		const article = (await page.locator('article.thread').boundingBox())!;
		expect(article.width).toBeGreaterThan(380);
		expect(
			await page.evaluate(() => (window as unknown as { e2eSplitSeen: string[] }).e2eSplitSeen)
		).toEqual([]);
	});
});
