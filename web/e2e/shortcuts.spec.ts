import { LABELS } from './mock/fixture.ts';
import { expect, list, openInbox, row, test } from './helpers.ts';

test.beforeEach(async ({ page }) => {
	await openInbox(page);
	await list(page).focus();
});

test('j and k move the selection', async ({ page }) => {
	await expect(row(page, 0)).toHaveAttribute('aria-selected', 'true');
	await page.keyboard.press('j');
	await page.keyboard.press('j');
	await expect(row(page, 2)).toHaveAttribute('aria-selected', 'true');
	await page.keyboard.press('k');
	await expect(row(page, 1)).toHaveAttribute('aria-selected', 'true');
	await expect(row(page, 2)).toHaveAttribute('aria-selected', 'false');
	const active = await list(page).getAttribute('aria-activedescendant');
	expect(await row(page, 1).getAttribute('id')).toBe(active);
});

test('j keeps the selection in view past the first screen', async ({ page }) => {
	for (let i = 0; i < 40; i++) await page.keyboard.press('j');
	await expect(row(page, 40)).toHaveAttribute('aria-selected', 'true');
	await expect(row(page, 40)).toBeInViewport();
});

test('o and Enter open the conversation, u goes back and keeps the selection', async ({ page }) => {
	await page.keyboard.press('j');
	await page.keyboard.press('j');
	const subject = await row(page, 2).locator('.subject').textContent();
	await page.keyboard.press('o');
	await expect(page).toHaveURL(/\/thread\/\d+$/);
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(subject!);
	await expect(page.getByText('3 of 100,000')).toBeVisible();
	await page.keyboard.press('u');
	await expect(page).toHaveURL(/\/inbox$/);
	await expect(row(page, 2)).toHaveAttribute('aria-selected', 'true');
	await list(page).focus();
	await page.keyboard.press('Enter');
	await expect(page).toHaveURL(/\/thread\/\d+$/);
});

test('j and k inside a conversation walk the list', async ({ page }) => {
	await page.keyboard.press('o');
	await expect(page.getByText('1 of 100,000')).toBeVisible();
	await page.keyboard.press('j');
	await expect(page.getByText('2 of 100,000')).toBeVisible();
	await page.keyboard.press('k');
	await expect(page.getByText('1 of 100,000')).toBeVisible();
	await page.keyboard.press('u');
	await expect(row(page, 0)).toHaveAttribute('aria-selected', 'true');
});

test('/ focuses search and Escape leaves it', async ({ page }) => {
	await page.keyboard.press('/');
	const search = page.getByRole('searchbox', { name: 'Search mail' });
	await expect(search).toBeFocused();
	await page.keyboard.type('j');
	await expect(search).toHaveValue('j');
	await expect(row(page, 0)).toHaveAttribute('aria-selected', 'true');
	await page.keyboard.press('Escape');
	await expect(search).not.toBeFocused();
});

test('? toggles the shortcut help', async ({ page }) => {
	await page.keyboard.press('?');
	const help = page.getByRole('dialog', { name: 'Keyboard shortcuts' });
	await expect(help).toBeVisible();
	await expect(help.getByText('Go to label')).toBeVisible();
	await page.keyboard.press('Escape');
	await expect(help).toBeHidden();
});

test('g then i goes to the inbox', async ({ page }) => {
	await page.goto('/search?q=is%3Astarred');
	await expect(page.getByRole('listbox', { name: /Search results/ })).toBeVisible();
	await page.locator('body').click({ position: { x: 5, y: 300 } });
	await page.keyboard.press('g');
	await page.keyboard.press('i');
	await expect(page).toHaveURL(/\/inbox$/);
});

test('g then l jumps to a label', async ({ page }) => {
	await page.keyboard.press('g');
	await page.keyboard.press('l');
	const jump = page.getByRole('dialog', { name: 'Go to label' });
	await expect(jump).toBeVisible();
	await page.keyboard.type('fin');
	await page.keyboard.press('Enter');
	const finance = LABELS.find((l) => l.name === 'Finance')!;
	await expect(page).toHaveURL(new RegExp(`/label/${finance.key}$`));
	await expect(page.getByRole('listbox', { name: 'Label Finance' })).toBeVisible();
	await expect(page.getByRole('link', { name: 'Finance' })).toHaveAttribute('aria-current', 'page');
});
