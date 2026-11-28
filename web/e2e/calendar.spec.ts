import type { Page } from '@playwright/test';
import { invitationThread, type InvitationScenario } from './mock/calendar.ts';
import { expect, signIn, test } from './helpers.ts';

declare global {
	interface Window {
		__pwned?: string;
	}
}

test.use({ locale: 'en-US', timezoneId: 'America/New_York' });

async function openInvitation(page: Page, scenario: InvitationScenario) {
	await signIn(page);
	await page.goto(`/thread/${invitationThread(scenario)}`);
	return page.getByRole('region', { name: 'Calendar invitation' });
}

test('card shows title, times with original TZ, organizer, attendees, recurrence', async ({
	page
}) => {
	const card = await openInvitation(page, 'request');
	await expect(card.getByRole('heading', { name: 'Product sync' })).toBeVisible();
	await expect(card).toContainText('Fri, Dec 4, 2026, 3:00 AM – 4:00 AM');
	await expect(card).toContainText(
		"Organizer's time: Fri, Dec 4, 2026, 9:00 AM – 10:00 AM (Europe/Zurich)"
	);
	await expect(card).toContainText('Every week on Friday');
	await expect(card).toContainText('Room 4.02, Bahnhofstrasse 1');
	await expect(card).toContainText('Dana Keller <dana@acme.example>');
	await expect(card.getByRole('listitem')).toHaveCount(5);
	await card.getByRole('button', { name: 'Show all 7 attendees' }).click();
	await expect(card.getByRole('listitem')).toHaveCount(7);
	await expect(card.getByRole('button', { name: 'Show fewer' })).toBeVisible();
});

test('a REQUEST offers the RSVP buttons', async ({ page }) => {
	const card = await openInvitation(page, 'request');
	for (const name of ['Accept', 'Tentative', 'Decline']) {
		await expect(card.getByRole('button', { name })).toBeEnabled();
	}
	await expect(card).not.toContainText('No calendar set up');
	await expect(card.getByRole('alert')).toHaveCount(0);
});

test('a CANCEL shows a cancelled banner and no actions', async ({ page }) => {
	const card = await openInvitation(page, 'cancel');
	await expect(card.getByRole('status')).toHaveText('This event was cancelled.');
	for (const name of ['Accept', 'Update calendar', 'Mark cancelled in calendar']) {
		await expect(card.getByRole('button', { name })).toHaveCount(0);
	}
});

test('a PUBLISH of an all-day event offers Add to calendar', async ({ page }) => {
	const card = await openInvitation(page, 'publish');
	await expect(card).toContainText('Thu, Dec 24, 2026 – Fri, Dec 25, 2026 · All day');
	await expect(card.getByRole('button', { name: 'Add to calendar' })).toBeEnabled();
	await expect(page.getByText('This message has more invitations than are shown.')).toBeVisible();
});

test('an oversized invitation says so instead of a card', async ({ page }) => {
	await openInvitation(page, 'too-large');
	await expect(page.getByText('Invitation too large to show.')).toBeVisible();
	await expect(page.getByRole('region', { name: 'Calendar invitation' })).toHaveCount(0);
});

test('a sender who is not the organizer is flagged', async ({ page }) => {
	const card = await openInvitation(page, 'spoof');
	await expect(card.getByRole('alert')).toHaveText('The sender is not the organizer.');
	await expect(card).toContainText('Dana Keller <ceo@corp.example> also listed as x@evil.example');
});

test('description and links render as text; links are safe', async ({ page }) => {
	const card = await openInvitation(page, 'request');
	await expect(card).toContainText('<img src=x onerror=');
	await expect(card).toContainText('<script>window.__pwned="script"</script>');
	await expect(card.locator('img, script')).toHaveCount(0);
	expect(await page.evaluate(() => window.__pwned)).toBeUndefined();
	for (const href of ['https://docs.example/agenda?id=7', 'https://meet.example/product-sync']) {
		const link = card.locator(`a[href="${href}"]`);
		await expect(link).toHaveAttribute('target', '_blank');
		await expect(link).toHaveAttribute('rel', 'noopener noreferrer');
	}
});

test('an outdated invitation is flagged and a floating time is not converted', async ({ page }) => {
	const card = await openInvitation(page, 'outdated');
	await expect(card).toContainText('Outdated invitation');
	await expect(card).toContainText('Fri, Dec 4, 2026, 9:00 AM – 10:00 AM');
	await expect(card).not.toContainText("Organizer's time");
});

test('an unreadable invitation says so', async ({ page }) => {
	await openInvitation(page, 'unparsable');
	await expect(page.getByRole('alert')).toHaveText("Couldn't read this invitation.");
});
