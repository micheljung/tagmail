import type { Page } from '@playwright/test';
import { ATTACKER } from './mock/render.ts';
import { XSS_INDEX, idAt } from './mock/fixture.ts';
import { expect, signIn, test } from './helpers.ts';

declare global {
	interface Window {
		__pwned?: string;
	}
}

const FRAME = 'iframe[title^="Message from"]';

async function openHostileMessage(page: Page): Promise<string[]> {
	const attacker: string[] = [];
	await page.route(`${ATTACKER}/**`, (route) => {
		attacker.push(route.request().url());
		return route.fulfill({ status: 204 });
	});
	await signIn(page);
	await page.goto(`/thread/${idAt(XSS_INDEX)}`);
	await expect(page.locator(FRAME).last()).toHaveAttribute(
		'sandbox',
		'allow-same-origin allow-popups allow-popups-to-escape-sandbox'
	);
	const frame = page.frameLocator(FRAME).last();
	await expect(frame.getByText('Nothing to see here.')).toBeVisible();
	await frame.locator('#jslink').click();
	await frame.locator('#submit').click();
	await frame.locator('summary').click();
	await page.waitForTimeout(300);
	return attacker;
}

async function expectNotPwned(page: Page) {
	expect(await page.evaluate(() => window.__pwned)).toBeUndefined();
	for (const f of page.frames()) {
		expect(await f.evaluate(() => window.__pwned).catch(() => undefined)).toBeUndefined();
	}
}

test('the sandbox alone stops script in email content', async ({ page }) => {
	await page.route('**/api/messages/*/render*', async (route) => {
		const resp = await route.fetch();
		const headers = { ...resp.headers() };
		delete headers['content-security-policy'];
		await route.fulfill({ response: resp, headers });
	});
	await openHostileMessage(page);
	await expectNotPwned(page);
});

test('with the render CSP, hostile email neither runs script nor reaches the network', async ({
	page
}) => {
	const attacker = await openHostileMessage(page);
	await expectNotPwned(page);
	expect(attacker).toEqual([]);
});

test('the app shell carries no inline script', async ({ request }) => {
	const resp = await request.get('/inbox');
	expect(resp.headers()['content-security-policy']).toContain("script-src 'self'");
	const html = await resp.text();
	expect(html).toMatch(/<script src="\/_app\/boot\.[0-9a-f]{12}\.js"><\/script>/);
	expect(html).not.toMatch(/<script(?![^>]*\bsrc=)[^>]*>/);
});
