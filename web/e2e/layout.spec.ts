import type { Page, TestInfo } from '@playwright/test';
import { NOW } from './mock/fixture.ts';
import { expect, openInbox, row, test, type CspViolation } from './helpers.ts';

const DESKTOP = { width: 1280, height: 800 };
/** Playwright's own animation and caret overrides are inline styles the CSP blocks in WebKit. */
const SCREENSHOT = { animations: 'allow', caret: 'initial', maxDiffPixelRatio: 0.001 } as const;

test.use({ viewport: DESKTOP, locale: 'en-US', timezoneId: 'UTC' });

async function expectScreenshot(
	page: Page,
	name: string,
	cspViolations: CspViolation[],
	testInfo: TestInfo
): Promise<void> {
	const before = cspViolations.length;
	// The message body renders in the host's own sans-serif, which differs between machines.
	await expect(page).toHaveScreenshot(name, { ...SCREENSHOT, mask: [page.locator('iframe')] });
	if (testInfo.project.name !== 'webkit') return;
	// WebKit's inspector injects an inline style element while it captures.
	const during = cspViolations.splice(before);
	expect(during.filter((v) => v.directive !== 'style-src-elem' || v.blocked !== 'inline')).toEqual(
		[]
	);
}

test.beforeEach(async ({ page }) => {
	await page.emulateMedia({ reducedMotion: 'reduce' });
	await page.clock.setFixedTime(new Date(NOW * 1000));
	await openInbox(page);
	await expect(page.locator('.live.on')).toBeVisible();
});

test('the desktop inbox keeps its layout', async ({ page, cspViolations }, testInfo) => {
	await expectScreenshot(page, 'inbox.png', cspViolations, testInfo);
});

test('the desktop thread keeps its layout', async ({ page, cspViolations }, testInfo) => {
	await row(page, 0).click();
	await expect(page).toHaveURL(/\/thread\/\d+$/);
	await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
	await expect(page.locator('iframe').first()).toBeVisible();
	await expectScreenshot(page, 'thread.png', cspViolations, testInfo);
});
