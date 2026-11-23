import type { Page } from '@playwright/test';
import { LABELS, idAt, rowAt } from './mock/fixture.ts';
import { expect, list, openInbox, row, test } from './helpers.ts';

function total(page: Page, n: number) {
	return expect(page.locator('.total')).toHaveText(
		`${n.toLocaleString('en-US')} conversation${n === 1 ? '' : 's'}`
	);
}

function toast(page: Page, text: string | RegExp) {
	return page.locator('[data-toast]').filter({ hasText: text });
}

async function mode(page: Page, mode: 'ok' | 'reject' | 'fail') {
	await page.request.post('/__mock/ops-mode', { data: { mode } });
}

/** A label none of the first rows carry, so tri-state checks start from a known state. */
function freshLabel(rows: number) {
	const used = new Set(Array.from({ length: rows }, (_, i) => rowAt(i).labels).flat());
	return LABELS.find((l) => !used.has(l.key))!;
}

test.beforeEach(async ({ page }) => {
	await openInbox(page);
	await list(page).focus();
});

test('e archives, the row leaves the inbox, and Undo puts it back', async ({ page }) => {
	await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
	await page.keyboard.press('e');
	await expect(toast(page, 'Archived 1 conversation')).toBeVisible();
	await total(page, 99_999);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(1)}`);
	await expect(row(page, 0)).toHaveAttribute('aria-selected', 'true');
	await toast(page, 'Archived 1 conversation').getByRole('button', { name: 'Undo' }).click();
	await total(page, 100_000);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
	await page.reload();
	await total(page, 100_000);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
});

test('archived threads stay out of the inbox after a reload', async ({ page }) => {
	await page.keyboard.press('e');
	await total(page, 99_999);
	await expect(page.locator('.unsynced')).toHaveCount(0);
	await page.reload();
	await total(page, 99_999);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(1)}`);
});

test('s stars, Shift+U and Shift+I flip read state', async ({ page }) => {
	const star = row(page, 0).locator('button.star');
	const starred = rowAt(0).starred;
	await page.keyboard.press('s');
	await expect(star).toHaveAttribute('aria-pressed', String(!starred));
	await page.keyboard.press('s');
	await expect(star).toHaveAttribute('aria-pressed', String(starred));
	await page.keyboard.press('Shift+U');
	await expect(row(page, 0).locator('.row')).toHaveClass(/unread/);
	await page.keyboard.press('Shift+I');
	await expect(row(page, 0).locator('.row')).not.toHaveClass(/unread/);
	await expect(toast(page, 'Marked 1 conversation as read')).toBeVisible();
});

test('x selects rows and # deletes all of them', async ({ page }) => {
	await page.keyboard.press('x');
	await page.keyboard.press('j');
	await page.keyboard.press('x');
	await expect(page.getByText('2 selected')).toBeVisible();
	await expect(row(page, 0).getByRole('checkbox')).toHaveAttribute('aria-checked', 'true');
	await page.keyboard.press('#');
	await expect(toast(page, 'Moved 2 conversations to Trash')).toBeVisible();
	await total(page, 99_998);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(2)}`);
});

test('shift-click ticks a range and the toolbar acts on it', async ({ page }) => {
	await row(page, 0).getByRole('checkbox').click();
	await row(page, 3)
		.getByRole('checkbox')
		.click({ modifiers: ['Shift'] });
	await expect(page.getByText('4 selected')).toBeVisible();
	await page.getByRole('toolbar').getByRole('button', { name: 'Mark as unread' }).click();
	for (let i = 0; i < 4; i++) await expect(row(page, i).locator('.row')).toHaveClass(/unread/);
	await expect(row(page, 4).getByRole('checkbox')).toHaveAttribute('aria-checked', 'false');
	await page.getByRole('toolbar').getByRole('button', { name: 'Archive' }).click();
	await total(page, 99_996);
	await expect(page.getByText('4 selected')).toHaveCount(0);
});

test('l labels with a searchable tri-state picker and can create labels', async ({ page }) => {
	const label = freshLabel(2);
	await page.keyboard.press('l');
	const picker = page.getByRole('dialog', { name: 'Label as' });
	await expect(picker).toBeVisible();
	await page.keyboard.type(label.name.slice(0, 4));
	await expect(picker.getByRole('option')).toHaveText([
		label.name,
		`Create “${label.name.slice(0, 4)}”`
	]);
	await page.keyboard.press('Enter');
	await expect(picker).toBeHidden();
	await expect(row(page, 0).locator('.chip', { hasText: label.name })).toBeVisible();

	await page.keyboard.press('x');
	await page.keyboard.press('j');
	await page.keyboard.press('x');
	await page.keyboard.press('l');
	const option = picker.getByRole('option', { name: label.name });
	await expect(option).toHaveAttribute('aria-checked', 'mixed');
	await option.click();
	await expect(option).toHaveAttribute('aria-checked', 'true');
	await option.click();
	await expect(option).toHaveAttribute('aria-checked', 'false');
	await option.click();
	await expect(option).toHaveAttribute('aria-checked', 'mixed');
	await option.click();
	await picker.getByRole('button', { name: 'Apply' }).click();
	await expect(row(page, 1).locator('.chip', { hasText: label.name })).toBeVisible();

	await page.keyboard.press('l');
	await page.keyboard.type('Urgent');
	const create = picker.getByRole('option', { name: 'Create “Urgent”' });
	await expect(create).toBeVisible();
	await page.keyboard.press('Enter');
	await expect(picker).toBeHidden();
	await expect(page.getByRole('link', { name: 'Urgent' })).toBeVisible();
	await expect(row(page, 0).locator('.chip', { hasText: 'Urgent' })).toBeVisible();
	await expect(row(page, 1).locator('.chip', { hasText: 'Urgent' })).toBeVisible();
	await page.getByRole('link', { name: 'Urgent' }).click();
	await total(page, 2);
});

test('v moves to a folder', async ({ page }) => {
	await page.keyboard.press('v');
	const picker = page.getByRole('dialog', { name: 'Move to' });
	await expect(picker.getByRole('option', { name: 'Inbox' })).toBeVisible();
	await page.keyboard.type('proj');
	await page.keyboard.press('Enter');
	await expect(toast(page, 'Moved 1 conversation to Projects')).toBeVisible();
	await total(page, 99_999);
});

test('a refused change rolls back and waits in the unsynced panel', async ({ page }) => {
	await mode(page, 'reject');
	await page.keyboard.press('e');
	const failure = toast(page, "Couldn't archive 1 conversation");
	await expect(failure).toBeVisible();
	await total(page, 100_000);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);

	await page.getByRole('button', { name: '1 unsynced' }).click();
	const panel = page.getByRole('dialog', { name: 'Unsynced changes' });
	const item = panel.getByRole('listitem').filter({ hasText: 'Archive 1 conversation' });
	await expect(item).toContainText('Failed');
	await expect(item).toContainText('the account is offline');

	await mode(page, 'ok');
	await item.getByRole('button', { name: 'Retry' }).click();
	await expect(panel.getByText('Everything is in sync')).toBeVisible();
	await page.keyboard.press('Escape');
	await total(page, 99_999);
	const state = (await (await page.request.get('/__mock/state')).json()) as {
		ops: { kind: string }[];
	};
	expect(state.ops.filter((o) => o.kind === 'archive')).toHaveLength(1);
});

test('an executor failure reverts the row and can be discarded', async ({ page }) => {
	await mode(page, 'fail');
	const star = row(page, 0).locator('button.star');
	const starred = rowAt(0).starred;
	await page.keyboard.press('s');
	await expect(star).toHaveAttribute('aria-pressed', String(!starred));
	await expect(toast(page, /Couldn't (un)?star 1 conversation/)).toBeVisible();
	await expect(star).toHaveAttribute('aria-pressed', String(starred));
	await page.getByRole('button', { name: '1 unsynced' }).click();
	const panel = page.getByRole('dialog', { name: 'Unsynced changes' });
	await expect(panel).toContainText('Mailbox is read-only');
	await panel.getByRole('button', { name: 'Discard' }).click();
	await expect(panel.getByText('Everything is in sync')).toBeVisible();
});

test('the conversation view takes the same shortcuts', async ({ page }) => {
	await page.keyboard.press('o');
	await expect(page).toHaveURL(new RegExp(`/thread/${idAt(0)}$`));
	const star = page.locator('.title button.star');
	await expect(star).toBeVisible();
	const pressed = await star.getAttribute('aria-pressed');
	await page.keyboard.press('s');
	await expect(star).toHaveAttribute('aria-pressed', pressed === 'true' ? 'false' : 'true');
	await page.keyboard.press('l');
	await expect(page.getByRole('dialog', { name: 'Label as' })).toBeVisible();
	await page.keyboard.press('Escape');
	await page.keyboard.press('e');
	await expect(page).toHaveURL(/\/inbox$/);
	await total(page, 99_999);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(1)}`);
});

test('labels can be created, colored, renamed and deleted in settings', async ({ page }) => {
	await page.getByRole('link', { name: 'Manage labels' }).click();
	await page.getByLabel('New label name').fill('Invoices');
	await page.getByRole('radiogroup', { name: 'New label color' }).getByRole('radio').nth(4).click();
	await page.getByRole('button', { name: 'Create' }).click();
	const item = page.locator('li[data-label="invoices"]');
	await expect(item).toContainText('Invoices');
	await expect(page.getByRole('navigation').getByRole('link', { name: 'Invoices' })).toBeVisible();

	await item.getByRole('button', { name: 'Color of Invoices' }).click();
	await item.getByRole('radio', { name: 'Color #8a4fd6' }).click();
	await expect(item.getByRole('button', { name: 'Color of Invoices' })).toHaveCSS(
		'background-color',
		'rgb(138, 79, 214)'
	);

	await item.getByRole('button', { name: 'Rename' }).click();
	await item.getByLabel('New name for Invoices').fill('Bills');
	await item.getByLabel('New name for Invoices').press('Enter');
	const bills = page.locator('li[data-label="bills"]');
	await expect(bills.getByRole('status')).toContainText('Renaming on the server');
	await expect(bills.getByRole('status')).toHaveCount(0);
	await expect(item).toHaveCount(0);
	await expect(page.getByRole('navigation').getByRole('link', { name: 'Bills' })).toBeVisible();

	await bills.getByRole('button', { name: 'Delete' }).click();
	await bills.getByRole('button', { name: 'Delete' }).click();
	await expect(bills).toHaveCount(0);
	await expect(page.getByRole('navigation').getByRole('link', { name: 'Bills' })).toHaveCount(0);
});

test('renaming a label keeps its conversations under the new name', async ({ page }) => {
	const label = LABELS.find((l) => l.key === 'finance')!;
	await page.goto(`/label/${label.key}`);
	const view = page.getByRole('listbox', { name: `Label ${label.name}` });
	await expect(view.locator('[data-loaded]').first()).toBeVisible();
	const count = await page.locator('.total').textContent();
	await page.getByRole('link', { name: 'Manage labels' }).click();
	const item = page.locator(`li[data-label="${label.key}"]`);
	await item.getByRole('button', { name: 'Rename' }).click();
	await item.getByLabel(`New name for ${label.name}`).fill('Money');
	await item.getByLabel(`New name for ${label.name}`).press('Enter');
	await expect(page.locator('li[data-label="money"]').getByRole('status')).toBeVisible();
	await page.getByRole('navigation').getByRole('link', { name: 'Money' }).click();
	await expect(page).toHaveURL(/\/label\/money$/);
	await expect(page.locator('.total')).toHaveText(count!);
});

test('the mailbox route lists a folder and More reveals the special ones', async ({ page }) => {
	await page.keyboard.press('#');
	await total(page, 99_999);
	const nav = page.getByRole('navigation');
	await nav.getByRole('link', { name: 'Projects' }).click();
	await expect(page.getByText('Projects is empty')).toBeVisible();
	await expect(nav.getByRole('link', { name: 'Trash' })).toHaveCount(0);
	await nav.getByRole('button', { name: 'More' }).click();
	await expect(nav.getByRole('link', { name: 'Spam' })).toBeVisible();
	await nav.getByRole('link', { name: 'Trash' }).click();
	await expect(page).toHaveURL(/\/mailbox\/4$/);
	await expect(nav.getByRole('link', { name: 'Trash' })).toHaveAttribute('aria-current', 'page');
	await total(page, 1);
	await expect(row(page, 0)).toHaveId(`thread-${idAt(0)}`);
	await list(page, 'Folder Trash').focus();
	await page.keyboard.press('v');
	await page.keyboard.type('inbox');
	await page.keyboard.press('Enter');
	await expect(page.getByText('Trash is empty')).toBeVisible();
	await page.goto('/inbox');
	await total(page, 100_000);
});

test('undo returns trashed threads from a label list to their own folders', async ({ page }) => {
	const label = LABELS.find((l) => l.key === 'travel')!;
	await page.goto(`/label/${label.key}`);
	const view = page.getByRole('listbox', { name: `Label ${label.name}` });
	await expect(view.locator('[data-loaded]').first()).toBeVisible();
	await view.focus();
	const first = await row(page, 0).getAttribute('id');
	await page.keyboard.press('#');
	await toast(page, 'Moved 1 conversation to Trash').getByRole('button', { name: 'Undo' }).click();
	await expect(row(page, 0)).toHaveId(first!);
	await page.goto('/inbox');
	await total(page, 100_000);
});
