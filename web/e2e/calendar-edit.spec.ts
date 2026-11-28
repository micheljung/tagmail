import type { Page } from '@playwright/test';
import type { CalEditDto } from '../../packages/api-types/src/index.ts';
import { expect, signIn, test } from './helpers.ts';

test.use({ locale: 'en-US', timezoneId: 'Europe/Zurich' });

const HOUR_PX = 48;
const PHONE = { width: 390, height: 844 };

async function open(page: Page, query: string) {
	await signIn(page);
	await page.request.post('/__mock/caldav', { data: { scenario: 'configured' } });
	await page.goto(`/calendar?${query}`);
}

async function edits(page: Page): Promise<CalEditDto[]> {
	return ((await (await page.request.get('/__mock/caldav')).json()) as { edits: CalEditDto[] })
		.edits;
}

const editor = (page: Page) => page.getByRole('dialog', { name: /event/ });
const chip = (page: Page, name: string) => page.locator('[data-event]', { hasText: name });

test('a click in an empty slot creates an event there with a reminder', async ({ page }) => {
	await open(page, 'view=week&date=2026-11-25');
	await page
		.locator('.column[data-day="2026-11-26"] .blank')
		.click({ position: { x: 20, y: 10 * HOUR_PX + 30 } });
	const dialog = editor(page);
	await expect(dialog.getByRole('heading', { name: 'New event' })).toBeVisible();
	await expect(dialog.getByLabel('Title')).toBeFocused();
	await page.keyboard.type('Planning');
	await expect(dialog.getByLabel('Starts')).toHaveValue('2026-11-26');
	await expect(dialog.getByLabel('at', { exact: true }).first()).toHaveValue('10:30');
	await expect(dialog.getByLabel('Time zone')).toHaveValue('Europe/Zurich');
	await dialog.getByLabel('Location').fill('Room 4');
	await dialog.getByLabel('Add reminder').selectOption({ label: '15 minutes before' });
	await expect(dialog.getByText('15 minutes before')).toBeVisible();
	await dialog.getByRole('button', { name: 'Save' }).click();
	await expect(dialog).toBeHidden();

	const placed = page.locator('.column[data-day="2026-11-26"]').locator('[data-event]', {
		hasText: 'Planning'
	});
	await expect(placed).toBeVisible();
	const [sent] = await edits(page);
	expect(sent).toMatchObject({
		kind: 'put',
		accountId: 1,
		calendarHref: '/dav/personal/',
		event: {
			summary: 'Planning',
			location: 'Room 4',
			start: '2026-11-26T10:30:00',
			end: '2026-11-26T11:30:00',
			allDay: false,
			tzid: 'Europe/Zurich',
			alarms: [15]
		}
	});
	expect(sent.href).toBeUndefined();
});

test('an all-day event and one in another time zone land where they belong', async ({ page }) => {
	await open(page, 'view=week&date=2026-11-25');
	await page.locator('.all-days [data-day="2026-11-28"] .blank').click();
	const dialog = editor(page);
	await expect(dialog.getByLabel('All day')).toBeChecked();
	await dialog.getByLabel('Title').fill('Holiday');
	await dialog.getByLabel('Ends').fill('2026-11-29');
	await dialog.getByRole('button', { name: 'Save' }).click();
	await expect(dialog).toBeHidden();
	const allDay = page.getByRole('group', { name: 'All day' });
	await expect(allDay.locator('[data-day="2026-11-29"]').getByText('Holiday')).toBeVisible();
	expect((await edits(page))[0].event).toMatchObject({
		start: '2026-11-28',
		end: '2026-11-30',
		allDay: true,
		tzid: null
	});

	await page.getByRole('button', { name: 'New event' }).click();
	await dialog.getByLabel('Title').fill('Call New York');
	await dialog.getByLabel('Starts').fill('2026-11-24');
	await dialog.getByLabel('at', { exact: true }).first().fill('09:00');
	await dialog.getByLabel('Ends').fill('2026-11-24');
	await dialog.getByLabel('at', { exact: true }).last().fill('10:00');
	await dialog.getByLabel('Time zone').selectOption('America/New_York');
	await dialog.getByRole('button', { name: 'Save' }).click();
	const call = page.locator('.column[data-day="2026-11-24"]').getByText('Call New York');
	await expect(call).toBeVisible();
	await expect(chip(page, 'Call New York').locator('.at')).toHaveText('3:00 PM');
});

test('editing changes only what was changed, and delete asks first', async ({ page }) => {
	await open(page, 'view=day&date=2026-11-25');
	await chip(page, 'Design review').click();
	const dialog = editor(page);
	await expect(dialog.getByRole('heading', { name: 'Edit event' })).toBeVisible();
	await expect(dialog.getByLabel('Title')).toHaveValue('Design review');
	await expect(dialog.getByLabel('at', { exact: true }).first()).toHaveValue('13:00');
	await expect(dialog.getByLabel('Time zone')).toHaveValue('UTC');
	await dialog.getByLabel('Title').fill('Design sign-off');
	await dialog.getByLabel('Add guest').fill('ann@example.com');
	await dialog.getByLabel('Add guest').press('Enter');
	await expect(dialog.getByText('ann@example.com')).toBeVisible();
	await dialog.getByRole('button', { name: 'Save' }).click();
	await expect(chip(page, 'Design sign-off')).toBeVisible();
	const [edit] = await edits(page);
	expect(edit).toMatchObject({
		kind: 'put',
		href: '/dav/work/review.ics',
		etag: '"0"',
		event: {
			summary: 'Design sign-off',
			start: '2026-11-25T13:00:00',
			end: '2026-11-25T14:30:00',
			tzid: 'UTC',
			attendees: [{ email: 'ann@example.com', name: null }]
		}
	});

	await chip(page, 'Design sign-off').click();
	await dialog.getByRole('button', { name: 'Delete', exact: true }).click();
	await dialog.getByRole('button', { name: 'Delete event' }).click();
	await expect(dialog).toBeHidden();
	await expect(chip(page, 'Design sign-off')).toHaveCount(0);
	expect((await edits(page))[1]).toMatchObject({ kind: 'delete', href: '/dav/work/review.ics' });
});

test('moving an event to another calendar and refusing a bad end', async ({ page }) => {
	await open(page, 'view=agenda&date=2026-11-25');
	await page.locator('.item[data-event="dentist"]').click();
	const dialog = editor(page);
	await dialog.getByLabel('Calendar').selectOption({ label: 'Work' });
	await dialog.getByLabel('at', { exact: true }).last().fill('14:00');
	await dialog.getByRole('button', { name: 'Save' }).click();
	await expect(dialog.getByRole('alert')).toHaveText('The event ends before it starts.');
	await dialog.getByLabel('at', { exact: true }).last().fill('16:00');
	await dialog.getByRole('button', { name: 'Save' }).click();
	await expect(dialog).toBeHidden();
	await expect(page.locator('.item[data-event="dentist"] .cal')).toHaveText('Work');
	expect(await edits(page)).toMatchObject([
		{ kind: 'move', href: '/dav/personal/dentist.ics', destCalendarHref: '/dav/work/' }
	]);
});

test('the keyboard opens, edits and closes the editor', async ({ page }) => {
	await open(page, 'view=agenda&date=2026-11-25');
	await page.locator('.item[data-event="call"]').focus();
	await page.keyboard.press('Enter');
	const dialog = editor(page);
	await expect(dialog.getByLabel('Title')).toHaveValue('Call with Sam');
	await page.keyboard.press('Escape');
	await expect(dialog).toHaveCount(0);
	await page.locator('.item[data-event="call"]').press('Enter');
	await dialog.getByLabel('Title').fill('Call with Sam and Kim');
	await dialog.getByLabel('Title').press('Enter');
	await expect(dialog).toBeHidden();
	await expect(page.locator('.item[data-event="call"] strong')).toHaveText('Call with Sam and Kim');
});

test('on a phone the editor fills the screen', async ({ page }) => {
	await page.setViewportSize(PHONE);
	await open(page, 'view=agenda&date=2026-11-25');
	await page.getByRole('button', { name: 'New event' }).click();
	const box = (await editor(page).boundingBox())!;
	expect([box.x, box.y, Math.round(box.width), Math.round(box.height)]).toEqual([
		0,
		0,
		PHONE.width,
		PHONE.height
	]);
	const save = await editor(page).getByRole('button', { name: 'Save' }).boundingBox();
	expect(save!.height).toBeGreaterThanOrEqual(44);
	expect(save!.y + save!.height).toBeLessThanOrEqual(PHONE.height);
});

test('an invited event keeps its guests and can be declined as it goes', async ({ page }) => {
	await open(page, 'view=agenda&date=2026-11-25');
	await page.locator('.item[data-event="workshop"]').click();
	const dialog = editor(page);
	await expect(dialog.getByText('boss@example.com organizes this event')).toBeVisible();
	await expect(dialog.getByLabel('Add guest')).toHaveCount(0);
	await dialog.getByRole('button', { name: 'Delete', exact: true }).click();
	await expect(dialog.getByRole('button', { name: 'Delete only' })).toBeVisible();
	await dialog.getByRole('button', { name: 'Delete and send decline' }).click();
	await expect(dialog).toBeHidden();
	await expect(page.locator('.item[data-event="workshop"]')).toHaveCount(0);
	expect(await edits(page)).toMatchObject([
		{ kind: 'delete', href: '/dav/personal/workshop.ics', sendDecline: true }
	]);
});

test('moving the start keeps the length', async ({ page }) => {
	await open(page, 'view=day&date=2026-11-25');
	await chip(page, 'Design review').click();
	const dialog = editor(page);
	await dialog.getByLabel('at', { exact: true }).first().fill('16:00');
	await expect(dialog.getByLabel('at', { exact: true }).last()).toHaveValue('17:30');
	await dialog.getByRole('button', { name: 'Save' }).click();
	await expect(dialog).toBeHidden();
	expect((await edits(page))[0].event).toMatchObject({
		start: '2026-11-25T16:00:00',
		end: '2026-11-25T17:30:00'
	});
});
