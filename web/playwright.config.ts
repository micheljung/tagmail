import { defineConfig, devices } from '@playwright/test';

const PORT = Number(process.env.E2E_PORT ?? 4180);
const MOBILE_ONLY = '**/mobile.spec.ts';
/** CDP-only checks (installability, push delivery). */
const CHROMIUM_ONLY = '**/pwa.spec.ts';
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
			use: { ...devices['Pixel 7'], launchOptions: { args: noSandbox } }
		},
		{ name: 'mobile-webkit', testMatch: MOBILE_ONLY, use: { ...devices['iPhone 13'] } }
	],
	webServer: {
		command: 'node e2e/mock/server.ts',
		url: `http://127.0.0.1:${PORT}/login`,
		env: { PORT: String(PORT) },
		reuseExistingServer: !process.env.CI,
		stdout: 'pipe'
	}
});
