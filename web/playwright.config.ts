import { defineConfig, devices } from '@playwright/test';

const PORT = Number(process.env.E2E_PORT ?? 4180);
const MOBILE_ONLY = '**/mobile.spec.ts';
/** CDP-only checks (installability, push delivery). */
const CHROMIUM_ONLY = '**/pwa.spec.ts';
/** Regenerates docs/images (`pnpm run screenshots`); never part of a normal run. */
const SCREENSHOTS = process.env.TAGMAIL_SCREENSHOTS === '1';
/*
 * Touch input differs per mobile project: Chromium drives real touch through
 * CDP (`@cdp-touch` blocks), WebKit has no CDP and sends synthetic events
 * (`@synthetic-touch` blocks). Each project drops the other's tag.
 */
const noSandbox = process.env.PW_NO_SANDBOX === '1' ? ['--no-sandbox'] : [];

export default defineConfig({
	testDir: 'e2e',
	testMatch: '**/*.spec.ts',
	snapshotPathTemplate: '{testDir}/__screenshots__/{testFilePath}-{projectName}/{arg}{ext}',
	fullyParallel: false,
	workers: 1,
	forbidOnly: !!process.env.CI,
	retries: 0,
	reporter: process.env.CI ? [['list'], ['html', { open: 'never' }]] : 'list',
	use: {
		baseURL: `http://127.0.0.1:${PORT}`,
		trace: 'retain-on-failure'
	},
	projects: [
		{
			name: 'chromium',
			testIgnore: MOBILE_ONLY,
			use: { ...devices['Desktop Chrome'], launchOptions: { args: noSandbox } }
		},
		{
			name: 'webkit',
			testIgnore: [MOBILE_ONLY, CHROMIUM_ONLY],
			use: { ...devices['Desktop Safari'] }
		},
		{
			name: 'mobile-chromium',
			testMatch: MOBILE_ONLY,
			grepInvert: /@synthetic-touch/,
			use: { ...devices['Pixel 7'], launchOptions: { args: noSandbox } }
		},
		{
			name: 'mobile-webkit',
			testMatch: MOBILE_ONLY,
			grepInvert: /@cdp-touch/,
			use: { ...devices['iPhone 13'] }
		},
		...(SCREENSHOTS
			? [
					{
						name: 'docs-screenshots',
						testMatch: '**/docs-screenshots.ts',
						use: { ...devices['Desktop Chrome'], launchOptions: { args: noSandbox } }
					}
				]
			: [])
	],
	webServer: {
		command: 'node e2e/mock/server.ts',
		url: `http://127.0.0.1:${PORT}/login`,
		env: { PORT: String(PORT) },
		reuseExistingServer: !process.env.CI,
		stdout: 'pipe'
	}
});
