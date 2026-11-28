import type { Page } from '@playwright/test';
import { invitationThread, type InvitationScenario } from './mock/calendar.ts';
import type { CalOutcome } from './mock/calOps.ts';
import { expect, signIn, test } from './helpers.ts';

test.use({ locale: 'en-US', timezoneId: 'America/New_York' });

async function openInvitation(page: Page, scenario: InvitationScenario) {
	await signIn(page);
	await page.goto(`/thread/${invitationThread(scenario)}`);
	return page.getByRole('region', { name: 'Calendar invitation' });
}

const outcome = (page: Page, next: CalOutcome) =>
	page.request.post('/__mock/calendar-outcome', { data: { outcome: next } });

const API = { headers: { 'X-Requested-With': 'tagmail' } };

function unsyncedPanel(page: Page) {
	return page.getByRole('dialog', { name: 'Unsynced changes' });
}

async function openUnsynced(page: Page) {
	await page.getByRole('button', { name: /1 unsynced/ }).click();
	return unsyncedPanel(page);
}

test('accepting queues the reply, shows it pending, then shows my answer', async ({ page }) => {
	const card = await openInvitation(page, 'request');
	await card.getByRole('button', { name: 'Accept' }).click();
	await expect(card.getByRole('status')).toContainText('Sending reply…');
	await expect(card.getByRole('button', { name: 'Accept' })).toBeDisabled();
	await expect(card.getByRole('status')).toHaveText('You accepted');
	await expect(card.getByRole('button', { name: 'Accept' })).toHaveAttribute(
		'aria-pressed',
		'true'
	);
	await expect(card.getByRole('button', { name: 'Decline' })).toHaveAttribute(
		'aria-pressed',
		'false'
	);
});

test('declining marks Decline and leaves the other answers open', async ({ page }) => {
	const card = await openInvitation(page, 'request');
	await card.getByRole('button', { name: 'Decline' }).click();
	await expect(card.getByRole('status')).toHaveText('You declined');
	await expect(card.getByRole('button', { name: 'Decline' })).toHaveAttribute(
		'aria-pressed',
		'true'
	);
	await expect(card.getByRole('button', { name: 'Tentative' })).toBeEnabled();
});

test('my earlier answer shows when the card opens', async ({ page }) => {
	const card = await openInvitation(page, 'manual');
	await expect(card.getByRole('button', { name: 'Accept' })).toHaveAttribute(
		'aria-pressed',
		'true'
	);
	await expect(card.getByRole('status')).toHaveText('You accepted');
});

test('an invitation file can be added to a chosen calendar', async ({ page }) => {
	await page.request.post('/__mock/caldav', { data: { scenario: 'configured' } });
	const card = await openInvitation(page, 'publish');
	const picker = card.getByRole('combobox', { name: 'Calendar' });
	await expect(picker.getByRole('option')).toHaveText(['Personal', 'Work']);
	await expect(picker).toHaveValue('1\n/dav/personal/');
	await picker.selectOption({ label: 'Work' });
	const request = page.waitForRequest('**/api/calendar/import');
	await card.getByRole('button', { name: 'Add to calendar' }).click();
	expect((await request).postDataJSON()).toMatchObject({
		accountId: 1,
		calendarHref: '/dav/work/'
	});
	await expect(card.getByRole('status')).toHaveText('In your calendar');
});

test('a stored event that is behind offers Update calendar', async ({ page }) => {
	const card = await openInvitation(page, 'manual');
	await card.getByRole('button', { name: 'Update calendar' }).click();
	await expect(card.getByRole('status')).toContainText('Updating calendar…');
	await expect(card.getByText('Applied')).toBeVisible();
	await expect(card.getByRole('button', { name: 'Update calendar' })).toHaveCount(0);
});

test('a CANCEL can be applied by hand', async ({ page }) => {
	const card = await openInvitation(page, 'cancel-manual');
	await expect(card.getByRole('button', { name: 'Accept' })).toHaveCount(0);
	await card.getByRole('button', { name: 'Mark cancelled in calendar' }).click();
	await expect(card.getByText('Updating calendar…')).toBeVisible();
	await expect(card.getByText('Applied')).toBeVisible();
	await expect(card.getByRole('button', { name: 'Mark cancelled in calendar' })).toHaveCount(0);
});

test('a calendar edit the server rejected can be re-applied', async ({ page }) => {
	const card = await openInvitation(page, 'request');
	await outcome(page, 'conflict');
	await card.getByRole('button', { name: 'Accept' }).click();
	await expect(card.getByRole('status')).toContainText(
		"Calendar changed on the server — your edit wasn't saved"
	);

	const panel = await openUnsynced(page);
	const item = panel.getByRole('listitem').filter({ hasText: 'Reply to invitation standup-1' });
	await expect(item).toContainText("Calendar changed on the server — your edit wasn't saved");
	await expect(item.getByRole('button', { name: 'Discard' })).toBeVisible();
	await item.getByRole('button', { name: 'Re-apply' }).click();
	await expect(panel.getByText('Everything is in sync')).toBeVisible();
	await page.keyboard.press('Escape');
	await expect(card.getByRole('status')).toHaveText('You accepted');
});

test('a calendar edit the server rejected can be discarded', async ({ page }) => {
	const card = await openInvitation(page, 'request');
	await outcome(page, 'conflict');
	await card.getByRole('button', { name: 'Decline' }).click();
	const panel = await openUnsynced(page);
	await panel
		.getByRole('listitem')
		.filter({ hasText: 'Reply to invitation standup-1' })
		.getByRole('button', { name: 'Discard' })
		.click();
	await expect(panel.getByText('Everything is in sync')).toBeVisible();
	await page.keyboard.press('Escape');
	await expect(card.getByRole('button', { name: 'Decline' })).toBeEnabled();
	await expect(card.getByRole('status')).toHaveCount(0);
});

test('a reply that went out but could not be saved says so and can be retried', async ({
	page
}) => {
	const card = await openInvitation(page, 'request');
	await outcome(page, 'reply-sent-not-saved');
	await card.getByRole('button', { name: 'Accept' }).click();
	const panel = await openUnsynced(page);
	const item = panel.getByRole('listitem').filter({ hasText: 'Reply to invitation standup-1' });
	await expect(item).toContainText('Reply sent, not saved to calendar');
	await item.getByRole('button', { name: 'Retry' }).click();
	await expect(panel.getByText('Everything is in sync')).toBeVisible();
});

test('a reply cancelled after the calendar was written says it was not sent', async ({ page }) => {
	const card = await openInvitation(page, 'request');
	await outcome(page, 'reply-cancelled');
	await card.getByRole('button', { name: 'Tentative' }).click();
	await expect(card.getByRole('status')).toContainText('Reply not sent');
	await expect(card.getByRole('button', { name: 'Send reply again' })).toBeVisible();
	await expect(card.getByRole('button', { name: 'Revert' })).toBeVisible();
});

test('a reply that was not sent can be reverted without sending one', async ({ page }) => {
	const card = await openInvitation(page, 'reply-not-sent');
	const request = page.waitForRequest('**/api/calendar/rsvp');
	await card.getByRole('button', { name: 'Revert' }).click();
	expect((await request).postDataJSON()).toMatchObject({
		partstat: 'accepted',
		sendReply: false
	});
	await expect(card.getByRole('status')).toHaveText('You accepted');
	await expect(card.getByRole('button', { name: 'Revert' })).toHaveCount(0);
});

test('a reply that was not sent can be sent again', async ({ page }) => {
	const card = await openInvitation(page, 'reply-not-sent');
	await expect(card.getByRole('status')).toContainText('Reply not sent');
	const request = page.waitForRequest('**/api/calendar/rsvp');
	await card.getByRole('button', { name: 'Send reply again' }).click();
	expect((await request).postDataJSON()).toMatchObject({ partstat: 'tentative' });
	await expect(card.getByRole('status')).toHaveText('You tentatively accepted');
	await expect(card.getByRole('button', { name: 'Send reply again' })).toHaveCount(0);
});

test('an update from another organizer warns and asks before updating', async ({ page }) => {
	const card = await openInvitation(page, 'organizer-changed');
	await expect(card.getByRole('alert')).toHaveText(
		'The organizer of this event changed — check before updating'
	);
	await card.getByRole('button', { name: 'Update calendar' }).click();
	const dialog = page.getByRole('dialog', { name: 'Continue with a different organizer?' });
	await expect(dialog).toContainText('lee@acme.example');
	await dialog.getByRole('button', { name: 'Cancel' }).click();
	await expect(dialog).toBeHidden();
	await expect(card.getByText('Applied')).toHaveCount(0);

	await card.getByRole('button', { name: 'Update calendar' }).click();
	const request = page.waitForRequest('**/api/calendar/apply');
	await dialog.getByRole('button', { name: 'Update calendar' }).click();
	expect((await request).postDataJSON()).toMatchObject({ allowOrganizerChange: true });
	await expect(card.getByText('Applied')).toBeVisible();
	await expect(card.getByRole('alert')).toHaveCount(0);
});

test('answering an update from another organizer asks first', async ({ page }) => {
	const card = await openInvitation(page, 'organizer-changed');
	await card.getByRole('button', { name: 'Decline' }).click();
	const dialog = page.getByRole('dialog', { name: 'Continue with a different organizer?' });
	const request = page.waitForRequest('**/api/calendar/rsvp');
	await dialog.getByRole('button', { name: 'Send reply' }).click();
	expect((await request).postDataJSON()).toMatchObject({
		partstat: 'declined',
		allowOrganizerChange: true
	});
	await expect(card.getByRole('status')).toHaveText('You declined');
});

test('without a calendar the reply still goes out and the card says where to set one up', async ({
	page
}) => {
	const card = await openInvitation(page, 'no-calendar');
	await expect(card).toContainText('No calendar set up');
	await expect(card.getByRole('link', { name: 'Set up CalDAV' })).toHaveAttribute(
		'href',
		'/settings/accounts/1#caldav-title'
	);
	await card.getByRole('button', { name: 'Accept' }).click();
	await expect(card.getByRole('status')).toContainText('Reply sent; not saved to a calendar');
});

test('the mock refuses what the server refuses', async ({ page }) => {
	const noCalendar = await openInvitation(page, 'no-calendar');
	await expect(noCalendar).toBeVisible();
	const thread = (await (
		await page.request.get(`/api/threads/${invitationThread('no-calendar')}`)
	).json()) as {
		messages: { id: number }[];
	};
	const messageId = String(thread.messages.at(-1)!.id);
	const post = (path: string, data: object) =>
		page.request.post(`/api/calendar/${path}`, { data, ...API });
	const refused = async (path: string, data: object, status: number, field?: string) => {
		const response = await post(path, data);
		expect(response.status()).toBe(status);
		if (field) expect(((await response.json()) as { field?: string }).field).toBe(field);
	};

	await refused('rsvp', { opId: 'bad id', messageId, partstat: 'accepted' }, 422, 'opId');
	await refused('rsvp', { opId: 'r-1', messageId, partstat: 'maybe' }, 422, 'partstat');
	await refused('rsvp', { opId: 'r-4', messageId, partstat: 'needs_action' }, 422, 'partstat');
	await refused(
		'rsvp',
		{ opId: 'r-5', messageId, partstat: 'accepted', sendReply: false },
		422,
		'messageId'
	);
	await refused(
		'rsvp',
		{ opId: 'r-2', messageId, partstat: 'accepted', identityId: 99 },
		422,
		'identityId'
	);
	await refused('import', { opId: 'i-1', messageId }, 422, 'calendarHref');
	await refused('import', { opId: 'i-2', messageId, accountId: 1 }, 422, 'calendarHref');
	await refused('apply', { opId: 'a-1', messageId }, 422, 'messageId');

	const takeover = String(
		(
			(await (
				await page.request.get(`/api/threads/${invitationThread('organizer-changed')}`)
			).json()) as { messages: { id: number }[] }
		).messages.at(-1)!.id
	);
	await refused('apply', { opId: 'a-2', messageId: takeover }, 422, 'allowOrganizerChange');
	await refused(
		'rsvp',
		{ opId: 'r-3', messageId: takeover, partstat: 'accepted' },
		422,
		'allowOrganizerChange'
	);

	const first = await post('rsvp', { opId: 'same', messageId, partstat: 'accepted' });
	expect(first.status()).toBe(202);
	expect((await post('rsvp', { opId: 'same', messageId, partstat: 'accepted' })).status()).toBe(
		200
	);
	await expect
		.poll(
			async () =>
				((await (await page.request.get('/api/ops/same')).json()) as { status: string }).status
		)
		.toBe('done');
	await page.request.delete('/api/ops/same', API);
	await refused('rsvp', { opId: 'same', messageId, partstat: 'accepted' }, 409);

	const running = await post('rsvp', { opId: 'busy', messageId, partstat: 'accepted' });
	expect(running.status()).toBe(202);
	expect((await page.request.post('/api/ops/busy/retry', API)).status()).toBe(409);
	expect((await page.request.delete('/api/ops/busy', API)).status()).toBe(204);
});
