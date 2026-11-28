import type { Locator, Page } from '@playwright/test';
import { expect, signIn, test } from './helpers.ts';

test.use({ locale: 'en-US', timezoneId: 'Europe/Zurich' });

const PHONE = { width: 390, height: 844 };
const WORK_GREEN = 'rgb(29, 122, 79)';

interface Seen {
	patches: Record<string, unknown>[];
	eventQueries: string[];
}

async function seen(page: Page): Promise<Seen> {
	return (await (await page.request.get('/__mock/caldav')).json()) as Seen;
}

async function open(page: Page, query: string) {
	await signIn(page);
	await page.request.post('/__mock/caldav', { data: { scenario: 'configured' } });
	await page.goto(`/calendar?${query}`);
}

const event = (scope: Page | Locator, uid: string) => scope.locator(`[data-event="${uid}"]`);
const column = (page: Page, day: string) => page.locator(`.column[data-day="${day}"]`);

async function box(target: Locator) {
	const b = await target.boundingBox();
	expect(b, 'rendered').not.toBeNull();
	return b!;
}

async function noSideScroll(page: Page) {
	const [scroll, client] = await page.evaluate(() => [
		document.documentElement.scrollWidth,
		document.documentElement.clientWidth
	]);
	expect(scroll).toBeLessThanOrEqual(client);
}

test('the switcher goes from mail to the calendar and back', async ({ page }) => {
	await signIn(page);
	await page.request.post('/__mock/caldav', { data: { scenario: 'configured' } });
	await page.goto('/inbox');
	const rail = page.getByRole('navigation', { name: 'Mailboxes' });
	await rail.getByRole('link', { name: 'Calendar' }).click();
	await expect(page).toHaveURL(/\/calendar$/);
	const calRail = page.getByRole('navigation', { name: 'Calendars' });
	await expect(calRail.getByRole('link', { name: 'Calendar' })).toHaveAttribute(
		'aria-current',
		'true'
	);
	await expect(calRail.getByRole('checkbox')).toHaveCount(3);
	await expect(page.getByRole('link', { name: 'Week' })).toHaveAttribute('aria-current', 'page');
	await calRail.getByRole('link', { name: 'Mail' }).click();
	await expect(page).toHaveURL(/\/inbox$/);
});

test('agenda lists the days with events, all-day ones first', async ({ page }) => {
	await open(page, 'view=agenda&date=2026-11-25');
	const agenda = page.getByRole('list', { name: 'Agenda' });
	const day = agenda.getByRole('listitem').filter({ hasText: 'Wednesday, November 25' });
	await expect(day.locator('.item strong')).toHaveText([
		'Design review',
		'Call with Sam',
		'Dentist'
	]);
	await expect(event(day, 'review')).toContainText('2:00 – 3:30 PM');
	await expect(event(day, 'review')).toContainText('Room 4');
	await expect(event(day, 'review')).toContainText('Work');
	const friday = agenda.getByRole('listitem').filter({ hasText: 'Friday, November 27' });
	await expect(friday.locator('.item strong')).toHaveText(['Team offsite', 'Dinner']);
	await expect(event(friday, 'offsite')).toContainText('All day');
	await expect(event(friday, 'dinner').locator('strong')).toHaveCSS(
		'text-decoration-line',
		'line-through'
	);
	await expect(agenda.getByRole('heading', { name: 'Monday, November 30' })).toBeVisible();
	const train = agenda.getByRole('listitem').filter({ hasText: 'Wednesday, December 2' });
	await expect(event(train, 'night-train')).toContainText('Dec 2, 9:00 PM – Dec 3, 8:00 AM');

	await page.getByRole('link', { name: 'Next' }).click();
	await expect(page).toHaveURL(/date=2026-12-25/);
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(
		'December 25, 2026 – January 23, 2027'
	);
});

test('day view places overlapping events side by side at their times', async ({ page }) => {
	await open(page, 'view=day&date=2026-11-25');
	await expect(page.getByRole('heading', { level: 1 })).toHaveText('November 25, 2026');
	const col = column(page, '2026-11-25');
	const review = await box(event(col, 'review'));
	const call = await box(event(col, 'call'));
	const dentist = await box(event(col, 'dentist'));
	const grid = await box(col);
	expect(call.x).toBeGreaterThanOrEqual(review.x + review.width - 1);
	expect(review.width).toBeLessThan(grid.width / 2 + 1);
	expect(dentist.width).toBeGreaterThan(grid.width * 0.9);
	expect(call.y - review.y).toBeCloseTo(24, 0);
	expect(Math.abs(review.height - 72)).toBeLessThan(4);
	await page.getByRole('link', { name: 'Previous' }).click();
	await expect(page).toHaveURL(/view=day&date=2026-11-24/);
	await expect(event(page, 'review')).toHaveCount(0);
});

test('on the day clocks go back, 9:00 sits at the 9 AM line', async ({ page }) => {
	await open(page, 'view=day&date=2026-10-25');
	const col = column(page, '2026-10-25');
	const review = await box(event(col, 'dst-review'));
	expect(review.y - (await box(col)).y).toBeCloseTo(9 * 48, 0);
	expect(Math.abs(review.height - 48)).toBeLessThan(4);
});

test('the events route refuses what the server refuses', async ({ page }) => {
	await signIn(page);
	for (const [query, status] of [
		['', 400],
		['from=1&to=x', 400],
		['from=10&to=10', 422],
		['from=0&to=5356800', 200],
		['from=0&to=5356801', 422],
		['from=-9223372036854775808&to=9223372036854775807', 422],
		['from=9223372036854775000&to=9223372036854775807', 422],
		['from=253402300800&to=253402300900', 422],
		['from=%2B5&to=10', 200],
		['from=0&to=10&calendars=%2Fcal%2Fwork%2F', 422]
	] as const) {
		const resp = await page.request.get(`/api/calendar/events?${query}`);
		expect(resp.status(), query).toBe(status);
	}
});

test('week view has seven days, an all-day row and pages by week', async ({ page }) => {
	await open(page, 'view=week&date=2026-11-25');
	await expect(page.getByRole('heading', { level: 1 })).toHaveText('November 23 – 29, 2026');
	await expect(page.locator('.head')).toHaveCount(7);
	await expect(event(column(page, '2026-11-23'), 'standup')).toContainText('Standup');
	const allDay = page.getByLabel('All day');
	await expect(event(allDay.locator('[data-day="2026-11-26"]'), 'offsite')).toBeVisible();
	await expect(event(allDay.locator('[data-day="2026-11-27"]'), 'offsite')).toBeVisible();
	await expect(event(allDay.locator('[data-day="2026-11-28"]'), 'offsite')).toHaveCount(0);

	await page.getByRole('link', { name: 'Previous' }).click();
	await expect(page).toHaveURL(/view=week&date=2026-11-18/);
	await expect(event(column(page, '2026-11-16'), 'standup')).toBeVisible();
	await expect(event(page, 'review')).toHaveCount(0);
	await page.getByRole('link', { name: 'Month' }).click();
	await expect(page).toHaveURL(/view=month&date=2026-11-18/);
});

test('month view fills six weeks and opens a day', async ({ page }) => {
	await open(page, 'view=month&date=2026-11-01');
	await expect(page.getByRole('heading', { level: 1 })).toHaveText('November 2026');
	await expect(page.getByRole('cell')).toHaveCount(42);
	const cell = (day: string) => page.locator(`.cell[data-day="${day}"]`);
	await expect(cell('2026-10-26')).toHaveClass(/other/);
	for (const monday of ['2026-11-02', '2026-11-09', '2026-11-16', '2026-11-23', '2026-11-30']) {
		await expect(event(cell(monday), 'standup')).toBeVisible();
	}
	await expect(cell('2026-11-25').locator('.chip')).toHaveCount(3);
	await expect(cell('2026-11-30').locator('.chip')).toHaveCount(2);
	await expect(event(cell('2026-11-30'), 'st-andrew')).toContainText("St Andrew's Day");
	await cell('2026-11-25').getByRole('link', { name: 'Wednesday, November 25' }).click();
	await expect(page).toHaveURL(/view=day&date=2026-11-25/);
});

test('each calendar has its colour and the colour can be changed', async ({ page }) => {
	await open(page, 'view=week&date=2026-11-25');
	await expect(event(page, 'review')).toHaveCSS('border-left-color', WORK_GREEN);
	await expect(event(page, 'dentist')).toHaveCSS('border-left-color', 'rgb(35, 80, 200)');
	await expect(event(page, 'offsite').first()).toHaveCSS('background-color', WORK_GREEN);

	const swatch = page.getByLabel('Colour of Work');
	await swatch.evaluate((el: HTMLInputElement) => {
		el.value = '#8a4fd6';
		el.dispatchEvent(new Event('change', { bubbles: true }));
	});
	await expect(event(page, 'review')).toHaveCSS('border-left-color', 'rgb(138, 79, 214)');
	await expect
		.poll(async () => (await seen(page)).patches.at(-1))
		.toEqual({ accountId: 1, href: '/dav/work/', color: '#8a4fd6' });
	await page.reload();
	await expect(event(page, 'review')).toHaveCSS('border-left-color', 'rgb(138, 79, 214)');
});

test('hiding a calendar drops its events and asks only for the shown ones', async ({ page }) => {
	await open(page, 'view=week&date=2026-11-25');
	await expect(event(page, 'review')).toBeVisible();
	await page.getByRole('checkbox', { name: 'Work' }).uncheck();
	await expect(event(page, 'review')).toHaveCount(0);
	await expect(event(page, 'standup')).toHaveCount(0);
	await expect(event(page, 'dentist')).toBeVisible();
	const { patches, eventQueries } = await seen(page);
	expect(patches.at(-1)).toEqual({ accountId: 1, href: '/dav/work/', visible: false });
	const last = new URLSearchParams(eventQueries.at(-1));
	expect(last.getAll('calendars').sort()).toEqual(['1:/dav/holidays/', '1:/dav/personal/']);

	await page.reload();
	await expect(page.getByRole('checkbox', { name: 'Work' })).not.toBeChecked();
	for (const name of ['Personal', 'Holidays']) {
		await page.getByRole('checkbox', { name }).uncheck();
	}
	await expect(page.locator('[data-event]')).toHaveCount(0);
	await page.getByRole('checkbox', { name: 'Work' }).check();
	await expect(event(page, 'review')).toBeVisible();
	await expect(event(page, 'dentist')).toHaveCount(0);
});

test('without calendars the page says how to add one', async ({ page }) => {
	await signIn(page);
	await page.goto('/calendar');
	await expect(page.getByText('No calendars yet')).toBeVisible();
	await expect(page.getByRole('main').getByRole('link', { name: 'Accounts' })).toBeVisible();
});

test.describe('on a phone', () => {
	test.use({ viewport: PHONE, hasTouch: true });

	test('the calendars live in the drawer', async ({ page }) => {
		await open(page, 'view=agenda&date=2026-11-25');
		await page.getByRole('button', { name: 'Menu', exact: true }).click();
		const drawer = page.getByRole('dialog', { name: 'Menu', exact: true });
		await expect(drawer.getByRole('link', { name: 'Calendar' })).toHaveAttribute(
			'aria-current',
			'true'
		);
		await drawer.getByRole('checkbox', { name: 'Personal' }).uncheck();
		await page.keyboard.press('Escape');
		await expect(event(page, 'dentist')).toHaveCount(0);
		await expect(event(page, 'review')).toBeVisible();
	});

	test('every view fits the screen', async ({ page }) => {
		await open(page, 'view=agenda&date=2026-11-25');
		const views = page.getByRole('navigation', { name: 'Calendar view' });
		expect((await box(views)).width).toBeGreaterThan(PHONE.width - 40);
		for (const name of ['Agenda', 'Day', 'Week', 'Month']) {
			const link = views.getByRole('link', { name });
			expect((await box(link)).height).toBeGreaterThanOrEqual(44);
			await link.click();
			await expect(link).toHaveAttribute('aria-current', 'page');
			await noSideScroll(page);
		}
		await expect(page.getByRole('cell')).toHaveCount(42);
		const dot = await box(event(page.locator('.cell[data-day="2026-11-25"]'), 'review'));
		expect([Math.round(dot.width), Math.round(dot.height)]).toEqual([7, 7]);
		await expect(page.locator('.cell[data-day="2026-11-25"] .label').first()).toBeHidden();

		await page.getByRole('link', { name: 'Week' }).click();
		const week = await box(page.locator('.scroll'));
		expect(week.x + week.width).toBeLessThanOrEqual(PHONE.width);
		const narrow = event(column(page, '2026-11-25'), 'call');
		await expect(narrow).toBeVisible();
		await expect(narrow.locator('.label')).toHaveCSS('white-space', 'nowrap');
		expect((await box(narrow)).height).toBeLessThan(30);
	});

	test('dark theme keeps the calendar colours on dark surfaces', async ({ page }) => {
		await page.emulateMedia({ colorScheme: 'dark' });
		await open(page, 'view=agenda&date=2026-11-25');
		await expect(page.getByRole('main')).toHaveCSS('background-color', 'rgb(20, 23, 28)');
		await expect(event(page, 'review')).toHaveCSS('border-left-color', WORK_GREEN);
	});
});
