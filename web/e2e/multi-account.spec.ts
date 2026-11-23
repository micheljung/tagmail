import type { Page } from '@playwright/test';
import { IDENTITY_B1, IDENTITY_B2, UNREACHABLE_HOST } from './mock/accounts.ts';
import { ACCOUNT_B, B_ALIAS, B_THREAD_COUNT, bIdAt, bRowAt } from './mock/fixture.ts';
import { expect, list, row, signIn, test } from './helpers.ts';

/** §6 row 13: several accounts, identities and signatures, against the mock's `multi` scenario. */

interface MockState {
	ops: { kind: string; label: string | null; accountId: number; threadIds: number[] }[];
	sent: { identityId: number; from?: string; html?: string }[];
	accounts: { id: number; name: string; email: string | null }[];
	identities: { id: number; signature: string | null }[];
	accountTests: { imapHost: string }[];
}

async function state(page: Page): Promise<MockState> {
	return (await (await page.request.get('/__mock/state')).json()) as MockState;
}

async function multi(page: Page): Promise<void> {
	await signIn(page);
	const resp = await page.request.post('/__mock/scenario', { data: { name: 'multi' } });
	expect(resp.ok()).toBe(true);
}

function marker(page: Page, account: string) {
	return page.getByRole('img', { name: `Account ${account}` });
}

function accountGroup(page: Page, account: string) {
	return page.getByRole('navigation', { name: 'Mailboxes' }).getByRole('list', { name: account });
}

test('the unified inbox mixes both accounts with markers; an account inbox shows its own', async ({
	page
}) => {
	await multi(page);
	await page.goto('/inbox');
	const inbox = list(page);
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
	await expect(
		inbox.locator('[role="option"]').filter({ has: marker(page, 'Bergmail') })
	).not.toHaveCount(0);
	await expect(
		inbox.locator('[role="option"]').filter({ has: marker(page, 'Personal') })
	).not.toHaveCount(0);
	await expect(page.getByRole('link', { name: 'All inboxes' })).toHaveAttribute(
		'aria-current',
		'page'
	);

	await accountGroup(page, 'Bergmail').getByRole('link', { name: 'Inbox' }).click();
	await expect(page).toHaveURL(/\/inbox\?account=2$/);
	const scoped = list(page, 'Inbox of Bergmail');
	await expect(page.getByText(`${B_THREAD_COUNT} conversations`)).toBeVisible();
	await expect(row(page, 0)).toContainText(bRowAt(0).subject);
	await expect(scoped.getByRole('img', { name: /^Account / })).toHaveCount(0);

	await accountGroup(page, 'Personal').getByRole('link', { name: 'Inbox' }).click();
	await expect(page).toHaveURL(/\/inbox\?account=1$/);
	await expect(page.getByText('100,000 conversations')).toBeVisible();
});

test('a reply to the alias comes from the alias with its signature, and From swaps it', async ({
	page
}) => {
	await multi(page);
	await page.goto(`/thread/${bIdAt(0)}`);
	await expect(page.getByRole('heading', { level: 1 })).toHaveText(bRowAt(0).subject);

	await page.keyboard.press('r');
	const reply = page.getByRole('region', { name: 'Reply' });
	const from = reply.getByLabel('From');
	const body = reply.getByRole('textbox', { name: 'Message body' });
	await expect(from).toHaveValue(String(IDENTITY_B2));
	await expect(from.locator('optgroup')).toHaveCount(2);
	await expect(body).toContainText('The Berg team');
	await expect(body).toContainText('wrote:');

	await from.selectOption(String(IDENTITY_B1));
	await expect(body).toContainText('Bergmail');
	await expect(body).not.toContainText('The Berg team');
	await from.selectOption(String(IDENTITY_B2));
	await expect(body).toContainText('The Berg team');
	await expect(body).not.toContainText('Michel Jung');

	await body.click();
	await page.keyboard.press('Control+Home');
	await page.keyboard.type('Count us in.');
	await page.keyboard.press('Control+Enter');
	await expect(page.locator('[data-toast]').filter({ hasText: 'Message sent' })).toBeVisible();
	const [sent] = (await state(page)).sent;
	expect(sent.identityId).toBe(IDENTITY_B2);
	expect(sent.from).toBe(B_ALIAS.email);
	expect(sent.html).toContain(`<div class="tagmail-signature" data-identity="${IDENTITY_B2}">`);
	expect(sent.html).toContain('The Berg team');
	expect(sent.html!.indexOf('Count us in.')).toBeLessThan(sent.html!.indexOf('The Berg team'));
	expect(sent.html!.indexOf('The Berg team')).toBeLessThan(sent.html!.indexOf('wrote:'));
});

test('a unified label lands in the thread’s own account and shows in its group', async ({
	page
}) => {
	await multi(page);
	await page.goto('/inbox');
	await expect(accountGroup(page, 'Bergmail').getByRole('link', { name: 'Shared' })).toHaveCount(0);
	const bRow = list(page)
		.locator('[role="option"]')
		.filter({ has: marker(page, 'Bergmail') })
		.first();
	await bRow.getByRole('checkbox', { name: 'Select conversation' }).click();
	await page.keyboard.press('l');
	const picker = page.getByRole('dialog', { name: 'Label as' });
	await picker.getByRole('option', { name: 'Shared' }).click();
	await picker.getByRole('button', { name: 'Apply' }).click();

	await expect(bRow.locator('.chip', { hasText: 'Shared' })).toBeVisible();
	await expect
		.poll(async () => (await state(page)).ops.filter((o) => o.kind === 'label.add'))
		.toEqual([expect.objectContaining({ label: 'shared', accountId: ACCOUNT_B })]);
	await expect(accountGroup(page, 'Bergmail').getByRole('link', { name: 'Shared' })).toBeVisible();
});

test('settings add an account after a connection test and edit a signature', async ({ page }) => {
	await multi(page);
	await page.goto('/inbox');
	await page.getByRole('link', { name: 'Accounts', exact: true }).click();
	await expect(page.getByRole('heading', { name: 'Accounts', level: 1 })).toBeVisible();
	await expect(page.locator('li[data-account]')).toHaveCount(2);

	await page.getByRole('button', { name: 'Add account' }).click();
	await page.getByLabel('Account name').fill('Work');
	await page.getByLabel('Username').fill('me@work.example');
	await expect(page.getByLabel('Send as (email address)')).toHaveValue('me@work.example');
	await page.getByLabel('Password').fill('hunter2hunter2');
	await page
		.getByRole('group', { name: 'Incoming (IMAP)' })
		.getByLabel('Server')
		.fill(UNREACHABLE_HOST);
	await page
		.getByRole('group', { name: 'Outgoing (SMTP)' })
		.getByLabel('Server')
		.fill('smtp.work.example');
	const add = page.getByRole('button', { name: 'Add account' });
	await expect(add).toBeDisabled();
	await page.getByRole('button', { name: 'Test connection' }).click();
	const result = page.getByRole('list', { name: 'Connection test' });
	await expect(result).toContainText('connection refused');
	await expect(add).toBeDisabled();

	await page
		.getByRole('group', { name: 'Incoming (IMAP)' })
		.getByLabel('Server')
		.fill('imap.work.example');
	await page.getByRole('button', { name: 'Test connection' }).click();
	await expect(result).toContainText('Connected in');
	await expect(result).not.toContainText('connection refused');
	await add.click();
	await expect(page.locator('li[data-account]')).toHaveCount(3);
	await expect(page.locator('li[data-account]').filter({ hasText: 'Work' })).toContainText(
		'me@work.example'
	);
	const after = await state(page);
	expect(after.accountTests.map((t) => t.imapHost)).toEqual([
		UNREACHABLE_HOST,
		'imap.work.example'
	]);
	expect(after.accounts.find((a) => a.name === 'Work')?.email).toBe('me@work.example');

	await page.locator(`li[data-account="${ACCOUNT_B}"]`).getByRole('link', { name: 'Edit' }).click();
	await expect(page.getByRole('heading', { name: 'Bergmail', level: 1 })).toBeVisible();
	await page.getByRole('button', { name: `Edit ${B_ALIAS.email}` }).click();
	const signature = page.getByRole('textbox', { name: 'Signature' });
	await expect(signature).toContainText('The Berg team');
	await signature.click();
	await page.keyboard.press('Control+End');
	await page.keyboard.type(' (on call)');
	await expect(page.getByTestId('signature-preview')).toContainText('(on call)');
	await page.getByRole('button', { name: 'Save identity' }).click();
	await expect(page.getByRole('button', { name: `Edit ${B_ALIAS.email}` })).toBeVisible();
	await expect
		.poll(async () => (await state(page)).identities.find((i) => i.id === IDENTITY_B2)?.signature)
		.toContain('(on call)');
});

test('an account that drops labels gets a banner linking to its settings', async ({ page }) => {
	await multi(page);
	await page.request.post('/__mock/degraded', {
		data: { accountId: ACCOUNT_B, codes: ['labels_degraded'] }
	});
	await page.goto('/inbox');
	const banner = page.locator('[data-notice="labels_degraded"]');
	await expect(banner).toContainText('Bergmail drops labels');
	await banner.getByRole('link', { name: 'Account settings' }).click();
	await expect(page).toHaveURL(`/settings/accounts/${ACCOUNT_B}`);
	await expect(page.getByRole('list', { name: 'Limitations' })).toContainText(
		'The server drops labels it does not know'
	);
});
