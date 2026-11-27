import { PASSWORD, SETUP_TOKEN, idAt, rowAt } from './mock/fixture.ts';
import { expect, list, openInbox, row, signIn, test } from './helpers.ts';

test('signing in sends the player back to where they were going', async ({ page }) => {
	await page.goto('/search?q=is%3Astarred');
	await expect(page).toHaveURL(/\/login\?next=/);
	await page.getByLabel('Password').fill('wrong password');
	await page.getByRole('button', { name: 'Sign in', exact: true }).click();
	await expect(page.getByRole('alert')).toHaveText('That password is not right.');
	await page.getByLabel('Password').fill(PASSWORD);
	await page.getByRole('button', { name: 'Sign in', exact: true }).click();
	await expect(page).toHaveURL(/\/search\?q=is%3Astarred$/);
	await expect(page.getByRole('link', { name: 'Starred' })).toHaveAttribute('aria-current', 'page');
});

test('a fresh install leads to setup', async ({ page }) => {
	await page.request.post('/__mock/fresh-install');
	await page.goto('/inbox');
	await expect(page).toHaveURL(/\/setup$/);
	await page.getByLabel('Setup token').fill('nope');
	await page.getByLabel('Password', { exact: true }).fill(PASSWORD);
	await page.getByLabel('Repeat password').fill(PASSWORD);
	await page.getByRole('button', { name: 'Create owner login' }).click();
	await expect(page.getByRole('alert')).toContainText('setup token is not valid');
	await page.getByLabel('Setup token').fill(SETUP_TOKEN);
	await page.getByRole('button', { name: 'Create owner login' }).click();
	await expect(page).toHaveURL(/\/inbox$/);
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
});

test('search offers operators and shows matching conversations', async ({ page }) => {
	await openInbox(page);
	const search = page.getByRole('searchbox', { name: 'Search mail' });
	await search.click();
	await page.getByRole('button', { name: /has:attachment/ }).click();
	await expect(search).toHaveValue('has:attachment');
	await search.press('Enter');
	await expect(page).toHaveURL(/\/search\?q=has%3Aattachment$/);
	const results = page.getByRole('listbox', { name: 'Search results for has:attachment' });
	await expect(results).toBeVisible();
	await expect(results.locator('[data-loaded]').first()).toBeVisible();
	const total = await page.locator('.total').textContent();
	expect(Number(total!.replace(/\D/g, ''))).toBeLessThan(100_000);
	await expect(
		results.locator('[data-loaded] [aria-label="Has attachment"]').first()
	).toBeVisible();
});

test('new mail from the change feed appears at the top', async ({ page }) => {
	await openInbox(page);
	await page.keyboard.press('j');
	const selected = await row(page, 1).locator('.subject').textContent();
	await expect(page.locator('.live.on')).toBeVisible();
	await page.request.post('/__mock/new-mail', { data: { subject: 'Hot off the change feed' } });
	await expect(row(page, 0).locator('.subject')).toHaveText('Hot off the change feed');
	await expect(row(page, 2)).toHaveAttribute('aria-selected', 'true');
	await expect(row(page, 2).locator('.subject')).toHaveText(selected!);
	await expect(page.getByText('100,001 conversations')).toBeVisible();
});

test('remote images stay blocked until the player asks', async ({ page }) => {
	await signIn(page);
	await page.goto(`/thread/${idAt(1)}`);
	const show = page.getByRole('button', { name: 'Show images' });
	await expect(show).toBeVisible();
	const frame = page.frameLocator('iframe[title^="Message from"]').last();
	await expect(frame.getByText('[image blocked]')).toBeVisible();
	await show.click();
	await expect(show).toBeHidden();
	await expect(frame.getByRole('img', { name: 'Spring collection' })).toBeVisible();
	const height = await page
		.locator('iframe[title^="Message from"]')
		.last()
		.evaluate((f) => f.clientHeight);
	expect(height).toBeGreaterThan(300);
});

test('the always-show list and attachments work per message', async ({ page }) => {
	await signIn(page);
	const id = Array.from({ length: 300 }, (_, i) => i).find(
		(i) => idAt(i) % 3 === 0 && rowAt(i).messageCount === 1 && rowAt(i).hasAttachment
	)!;
	await page.goto(`/thread/${idAt(id)}`);
	const sender = rowAt(id).senders[0].email;
	await page.getByRole('button', { name: `Always show from ${sender}` }).click();
	await expect(page.getByRole('button', { name: 'Show images' })).toBeHidden();
	const frame = page.frameLocator('iframe[title^="Message from"]').last();
	await expect(frame.getByRole('img', { name: 'Spring collection' })).toBeVisible();
	const file = page.getByRole('list', { name: 'Attachments' }).getByRole('link');
	await expect(file).toContainText('Statement-November.pdf');
	await expect(file).toContainText('243 KB');
	expect(await file.getAttribute('href')).toMatch(/download=1$/);
});

test('a merged conversation redirects to the thread it joined', async ({ page }) => {
	await signIn(page);
	await page.goto('/thread/999999');
	await expect(page).toHaveURL(new RegExp(`/thread/${idAt(0)}$`));
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(rowAt(0).subject);
});

test('devices and sessions can be listed, revoked and paired', async ({ page }) => {
	await signIn(page);
	await page.goto('/settings/devices');
	await expect(page.getByRole('heading', { name: 'Devices & sessions' })).toBeVisible();
	await expect(page.getByText('This browser')).toBeVisible();
	const main = page.getByRole('main');
	await main.getByRole('button', { name: 'Sign out' }).click();
	await expect(main.getByRole('button', { name: 'Sign out' })).toHaveCount(0);
	await page.getByRole('button', { name: 'Pair a device' }).click();
	await expect(page.getByLabel(/^Pairing code/)).toContainText('K7QD-2XPM');
	await expect(page.getByRole('img', { name: 'Pairing QR code' })).toBeVisible();
	await expect(page.getByText(/Expires in [45]:\d\d/)).toBeVisible();
	await page.getByRole('button', { name: 'Revoke' }).click();
	await expect(page.getByText('No desktop app is paired yet.')).toBeVisible();
});

test('theme switch persists across reloads', async ({ page }) => {
	await openInbox(page);
	await page.emulateMedia({ colorScheme: 'light' });
	await page.getByRole('button', { name: 'Switch light or dark theme' }).click();
	await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
	await page.reload();
	await expect(page.locator('html')).toHaveAttribute('data-theme', 'dark');
	const bg = await page.evaluate(() => getComputedStyle(document.body).backgroundColor);
	expect(bg).toBe('rgb(12, 14, 17)');
});

test('phone width shows one column with a menu drawer', async ({ page }) => {
	await page.setViewportSize({ width: 390, height: 844 });
	await openInbox(page);
	const nav = page.getByRole('navigation', { name: 'Mailboxes' });
	await expect(nav).not.toBeInViewport();
	const rowBox = await row(page, 0).boundingBox();
	expect(rowBox!.width).toBeGreaterThan(380);
	expect(rowBox!.height).toBe(76);
	await page.getByRole('button', { name: 'Menu' }).click();
	await expect(nav).toBeInViewport();
	await nav.getByRole('link', { name: 'Finance' }).click();
	await expect(page).toHaveURL(/\/label\/finance$/);
	await expect(nav).not.toBeInViewport();
	await expect(page.getByRole('listbox', { name: 'Label Finance' })).toBeVisible();
	await expect(list(page)).toHaveCount(0);
});

test('clicking a row while others are ticked still opens the thread', async ({ page }) => {
	await openInbox(page);
	const before = await page.evaluate(() => history.length);
	for (const index of [0, 1]) {
		await row(page, index).getByRole('checkbox', { name: 'Select conversation' }).click();
		await expect(row(page, index).locator('.check')).toHaveAttribute('aria-checked', 'true');
	}
	await expect(page.locator('.total')).toHaveText('2 selected');
	await row(page, 3).locator('.subject').click();
	await expect(page).toHaveURL(/\/thread\/\d+$/);
	// Only the thread was pushed: no selection entry.
	expect(await page.evaluate(() => history.length)).toBe(before + 1);
	expect(await page.evaluate(() => JSON.stringify(history.state))).not.toContain('selection');
});
