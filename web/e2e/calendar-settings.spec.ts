import type { Page } from '@playwright/test';
import { ACCOUNT_B } from './mock/fixture.ts';
import { AUTHSERV_SUGGESTION, DAV_HOST } from './mock/calendar.ts';
import { expect, signIn, test } from './helpers.ts';

interface Seen {
	puts: { accountId: number; body: Record<string, unknown> }[];
	patches: Record<string, unknown>[];
	confirms: unknown[];
	settings: Record<string, { authservId: string | null; defaultCalendarHref: string | null }>;
}

async function seen(page: Page): Promise<Seen> {
	return (await (await page.request.get('/__mock/caldav')).json()) as Seen;
}

async function open(page: Page, scenario: 'unconfigured' | 'configured', account = 1) {
	await signIn(page);
	await page.request.post('/__mock/caldav', { data: { scenario } });
	await page.goto(`/settings/accounts/${account}`);
	return page.getByRole('region', { name: 'Calendar' });
}

test('discovery asks before the mail login goes to another host', async ({ page }) => {
	const cal = await open(page, 'unconfigured');
	await expect(cal.getByLabel('Calendar status')).toContainText('Not set up');

	await cal.getByRole('button', { name: 'Discover' }).click();
	const dialog = page.getByRole('dialog', { name: `Send your mail login to ${DAV_HOST}?` });
	await expect(dialog).toContainText('sends your mail username and password');
	await expect(cal.getByLabel('Calendar status')).toContainText('Waiting for you to confirm');

	await dialog.getByRole('button', { name: 'Cancel' }).click();
	await expect(dialog).toBeHidden();
	await expect(cal.getByRole('list', { name: 'Calendars' })).toHaveCount(0);

	await cal.getByRole('button', { name: 'Discover' }).click();
	await dialog.getByRole('button', { name: `Send login to ${DAV_HOST}` }).click();
	await expect(dialog).toBeHidden();
	expect((await seen(page)).confirms).toEqual([{ host: DAV_HOST }]);
	await expect(cal.getByLabel('Calendar status')).toContainText('Connected');
	await expect(cal.getByLabel('Calendar status')).toContainText('detected: server');
	await expect(cal.getByRole('list', { name: 'Calendars' }).getByRole('listitem')).toHaveCount(3);
});

test('allowing http shows a warning', async ({ page }) => {
	const cal = await open(page, 'unconfigured');
	const warning = cal.getByRole('alert');
	await expect(warning).toHaveCount(0);
	await cal.getByRole('checkbox', { name: /unencrypted/ }).check();
	await expect(warning).toContainText('can be read on the network');
	await cal.getByRole('button', { name: 'Save calendar', exact: true }).click();
	await expect.poll(async () => (await seen(page)).puts.at(-1)?.body.allowInsecure).toBe(true);
	await cal.getByRole('checkbox', { name: /unencrypted/ }).uncheck();
	await expect(warning).toHaveCount(0);
});

test('an http URL without the checkbox shows the server error', async ({ page }) => {
	const cal = await open(page, 'unconfigured');
	await cal.getByLabel('Calendar server URL').fill('http://cal.example/dav');
	await cal.getByRole('button', { name: 'Save calendar', exact: true }).click();
	await expect(cal.getByRole('alert')).toContainText('http needs the unencrypted connection');
	expect((await seen(page)).puts).toEqual([]);
});

test('the dialog names the calendar login when one is set', async ({ page }) => {
	const cal = await open(page, 'unconfigured');
	await cal.getByLabel('Login (if not your mail login)').fill('dav-user');
	await cal.getByLabel('Calendar password').fill('s3cret');
	await cal.getByRole('button', { name: 'Discover' }).click();
	await expect(page.getByRole('dialog')).toContainText('Send your calendar login');
});

test('trust is manual until confirmed', async ({ page }) => {
	const cal = await open(page, 'configured');
	const field = cal.getByLabel('Trusted sender id');
	await expect(field).toHaveValue('');
	await expect(cal.getByText('nothing is applied automatically')).toBeVisible();
	await expect(cal.locator('[data-suggestion]')).toHaveText(AUTHSERV_SUGGESTION);
	expect((await seen(page)).settings[1].authservId).toBeNull();

	await cal.getByRole('button', { name: 'Confirm' }).click();
	await expect(field).toHaveValue(AUTHSERV_SUGGESTION);
	await expect(cal.locator('[data-suggestion]')).toHaveCount(0);
	expect((await seen(page)).settings[1].authservId).toBe(AUTHSERV_SUGGESTION);

	await field.fill('');
	await cal.getByRole('button', { name: 'Turn off' }).click();
	await expect.poll(async () => (await seen(page)).settings[1].authservId).toBeNull();
	const last = (await seen(page)).puts.at(-1)!.body;
	expect(last.authservId).toBeNull();
});

test('saving the connection leaves the trusted id alone', async ({ page }) => {
	const cal = await open(page, 'configured');
	await cal.getByRole('button', { name: 'Confirm' }).click();
	await expect(cal.getByLabel('Trusted sender id')).toHaveValue(AUTHSERV_SUGGESTION);
	await cal.getByLabel('Invitations are sent by').selectOption('client');
	await cal.getByRole('button', { name: 'Save calendar', exact: true }).click();
	await expect.poll(async () => (await seen(page)).puts.length).toBe(2);
	const { puts, settings } = await seen(page);
	expect('authservId' in puts[1].body).toBe(false);
	expect(settings[1].authservId).toBe(AUTHSERV_SUGGESTION);
});

test('a changed login needs the password again', async ({ page }) => {
	const cal = await open(page, 'configured');
	const password = cal.getByLabel('Calendar password');
	const save = cal.getByRole('button', { name: 'Save calendar', exact: true });
	await expect(password).toBeDisabled();

	await cal.getByLabel('Login (if not your mail login)').fill('dav-user');
	await expect(cal.getByText('Enter the password again')).toBeVisible();
	await expect(password).toHaveValue('');
	await expect(save).toBeDisabled();

	await password.fill('s3cret');
	await save.click();
	await expect.poll(async () => (await seen(page)).puts.at(-1)?.body.password).toBe('s3cret');
	await expect(password).toHaveValue('');
	await expect(cal.getByText('Enter the password again')).toHaveCount(0);
	await expect(password).toHaveAttribute('placeholder', 'Unchanged');
});

test('settings default + fallback: the default calendar changes', async ({ page }) => {
	const cal = await open(page, 'configured');
	const list = cal.getByRole('list', { name: 'Calendars' });
	await expect(list.locator('[data-calendar="/dav/personal/"]')).toContainText('Default');
	await expect(list.locator('[data-calendar="/dav/holidays/"]')).toContainText('Read only');

	const picker = cal.getByLabel('Default calendar');
	await expect(picker.locator('option')).toHaveCount(2);
	await picker.selectOption('/dav/work/');
	await expect(list.locator('[data-calendar="/dav/work/"]')).toContainText('Default');
	await expect(list.locator('[data-calendar="/dav/personal/"]')).not.toContainText('Default');
	const { patches } = await seen(page);
	expect(patches).toEqual([{ accountId: 1, href: '/dav/work/', default: true }]);

	await list
		.locator('[data-calendar="/dav/work/"]')
		.getByRole('checkbox', { name: 'Show' })
		.uncheck();
	await expect
		.poll(async () => (await seen(page)).patches.at(-1))
		.toEqual({ accountId: 1, href: '/dav/work/', visible: false });
});

test('an account without a calendar picks a fallback account', async ({ page }) => {
	await signIn(page);
	await page.request.post('/__mock/scenario', { data: { name: 'multi' } });
	await page.request.post('/__mock/caldav', { data: { scenario: 'configured' } });
	await page.goto(`/settings/accounts/${ACCOUNT_B}`);
	const cal = page.getByRole('region', { name: 'Calendar' });
	await expect(cal.getByLabel('Calendar status')).toContainText('Not set up');
	await expect(cal.getByRole('list', { name: 'Calendars' })).toHaveCount(0);

	await cal.getByLabel('Fallback account').selectOption({ label: 'Personal' });
	await expect
		.poll(async () => (await seen(page)).puts.at(-1))
		.toMatchObject({
			accountId: ACCOUNT_B,
			body: { fallbackAccountId: 1 }
		});
	await expect(cal.getByLabel('Fallback account')).toHaveValue('1');
});
