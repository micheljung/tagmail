import type { Page } from '@playwright/test';
import { idAt } from './mock/fixture.ts';
import { expect, signIn, test } from './helpers.ts';

declare global {
	interface Window {
		__tauriEmit(event: string, payload: unknown): void;
		__tauriListening(event: string): number;
	}
}

/**
 * Runs the SPA as the desktop app does: fake Tauri globals (event listen
 * only, like the `core:event:default` capability) and the mock's
 * `/api/desktop/*` routes.
 */
async function asDesktop(page: Page, state: Record<string, unknown> = {}): Promise<void> {
	await page.request.post('/__mock/desktop', { data: state });
	await page.addInitScript(() => {
		const callbacks = new Map<number, (event: unknown) => void>();
		const listeners = new Map<string, number[]>();
		let next = 1;
		const w = window as unknown as Record<string, unknown>;
		w.__TAURI_INTERNALS__ = {
			transformCallback(callback: (event: unknown) => void) {
				const id = next++;
				callbacks.set(id, callback);
				return id;
			},
			unregisterCallback(id: number) {
				callbacks.delete(id);
			},
			invoke(cmd: string, args: { event: string; handler: number }) {
				if (cmd === 'plugin:event|listen') {
					listeners.set(args.event, [...(listeners.get(args.event) ?? []), args.handler]);
					return Promise.resolve(args.handler);
				}
				if (cmd === 'plugin:event|unlisten') return Promise.resolve();
				return Promise.reject(new Error(`unexpected command ${cmd}`));
			}
		};
		w.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
			unregisterListener(event: string, id: number) {
				listeners.set(
					event,
					(listeners.get(event) ?? []).filter((h) => h !== id)
				);
			}
		};
		w.__tauriListening = (event: string) => (listeners.get(event) ?? []).length;
		w.__tauriEmit = (event: string, payload: unknown) => {
			for (const id of listeners.get(event) ?? []) callbacks.get(id)?.({ event, id, payload });
		};
	});
}

async function emit(page: Page, event: string, payload: unknown): Promise<void> {
	await expect.poll(() => page.evaluate((e) => window.__tauriListening(e), event)).toBe(1);
	await page.evaluate(([e, p]) => window.__tauriEmit(e as string, p), [event, payload]);
}

function banner(page: Page, kind: string) {
	return page.locator(`[data-desktop="${kind}"]`);
}

test('offline: the open error shows verbatim and search notes older results', async ({ page }) => {
	await asDesktop(page, { online: false });
	await signIn(page);
	await page.goto('/inbox');
	await expect(banner(page, 'offline')).toHaveText(/Offline\. Showing the mail on this device/);
	await page.goto(`/thread/${idAt(0)}`);
	await expect(banner(page, 'offline')).toHaveText('Older message needs a connection');
	await page.goto('/search?q=invoice');
	await expect(page.locator('[data-partial]')).toHaveText('Older results need a connection');
});

test('online search has no partial note', async ({ page }) => {
	await asDesktop(page);
	await signIn(page);
	await page.goto('/search?q=invoice');
	await expect(page.getByRole('option').first()).toBeVisible();
	await expect(page.locator('[data-partial]')).toHaveCount(0);
	await expect(banner(page, 'offline')).toHaveCount(0);
});

test('bootstrap progress, then update-required blocks syncing', async ({ page }) => {
	await asDesktop(page, { phase: { kind: 'bootstrapping', done: 40, total: 100 } });
	await signIn(page);
	await page.goto('/inbox');
	await expect(banner(page, 'bootstrap')).toContainText('40%');
	await emit(page, 'desktop:update-required', {
		server: { major: 2, minor: 0 },
		minClient: { major: 2, minor: 0 }
	});
	const update = banner(page, 'update');
	await expect(update).toHaveAttribute('role', 'alert');
	await expect(update).toContainText('needs app version 2.0 or newer');
	await expect(update.getByRole('link', { name: 'Download' })).toHaveAttribute(
		'href',
		'https://github.com/micheljung/tagmail/releases'
	);
});

test('an unpaired app pairs from a tagmail://pair link', async ({ page }) => {
	await signIn(page);
	await asDesktop(page, { paired: false });
	await page.goto('/inbox');
	await expect(page).toHaveURL(/\/pair$/);
	await page.getByLabel('Server address').fill('https://mail.example.com');
	await page.getByLabel('Pairing code').fill('WRONG123');
	await page.getByRole('button', { name: 'Connect' }).click();
	await expect(page.getByRole('alert')).toHaveText('That code is wrong or expired');
	await emit(page, 'desktop:pair', { server: 'https://other.example.com', code: 'K7QD2XPM' });
	await expect(page.getByLabel('Server address')).toHaveValue('https://other.example.com');
	await expect(page.getByLabel('Pairing code')).toHaveValue('K7QD2XPM');
	await page.getByRole('button', { name: 'Connect' }).click();
	await expect(page).toHaveURL(/\/inbox$/);
	await expect(page.getByRole('link', { name: 'This device' })).toBeVisible();
	await expect(page.getByRole('link', { name: /Devices/ })).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Sign out' })).toHaveCount(0);
});

test('a mailto link opens a prefilled compose window', async ({ page }) => {
	await asDesktop(page);
	await signIn(page);
	await page.goto('/inbox');
	await emit(page, 'desktop:mailto', {
		to: ['ana@sousa.family'],
		cc: [],
		bcc: [],
		subject: 'From a link',
		body: 'Hello\nthere'
	});
	const win = page.locator('section.window');
	await expect(win).toBeVisible();
	await expect(win.locator('.chip')).toHaveText(/ana@sousa\.family/);
	await expect(win.getByLabel('Subject')).toHaveValue('From a link');
	await expect(win.getByRole('textbox', { name: 'Message body' })).toHaveText(/Hello\s*there/);
});

test('desktop settings save through the local routes', async ({ page }) => {
	await asDesktop(page);
	await signIn(page);
	await page.goto('/settings/desktop');
	await expect(page.getByText('https://mail.example.com')).toBeVisible();
	const notify = page.getByLabel('Notify me about new mail');
	await expect(notify).toBeChecked();
	const saved = page.waitForResponse(
		(r) => r.url().endsWith('/api/desktop/settings') && r.request().method() === 'PATCH'
	);
	await notify.uncheck();
	expect(((await saved).request().postDataJSON() as { notify: boolean }).notify).toBe(false);
	await expect(page.getByLabel('Show sender and subject in notifications')).toBeDisabled();
	await page.getByRole('button', { name: 'Disconnect this device' }).click();
	await expect(page).toHaveURL(/\/pair$/);
});

test('the browser build keeps the web transport and pages', async ({ page }) => {
	await signIn(page);
	await page.goto('/inbox');
	await expect(page.getByRole('link', { name: /Devices/ })).toBeVisible();
	await expect(page.getByRole('link', { name: 'This device' })).toHaveCount(0);
	await expect(page.locator('[data-desktop]')).toHaveCount(0);
});
