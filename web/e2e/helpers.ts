import { expect, test as base, type Page } from '@playwright/test';
import { PASSWORD } from './mock/fixture.ts';

export interface CspViolation {
	directive: string;
	blocked: string;
	target: string;
	page: string;
}

/**
 * SvelteKit's route announcer carries an inline style attribute in its
 * template; the violation reports the document as its target. app.css
 * styles the announcer instead.
 */
const KNOWN_INLINE_STYLE_TARGETS = new Set(['svelte-announcer', '']);

export const test = base.extend<{ cspViolations: CspViolation[] }>({
	cspViolations: [
		async ({ page }, use) => {
			await page.request.post('/__mock/reset');
			const found: CspViolation[] = [];
			await page.exposeFunction('__reportCsp', (v: CspViolation) => found.push(v));
			await page.addInitScript(() => {
				document.addEventListener('securitypolicyviolation', (e) => {
					const target = e.target instanceof Element ? e.target.id : '';
					void (
						window as unknown as { __reportCsp: (v: CspViolation) => Promise<void> }
					).__reportCsp({
						directive: e.violatedDirective,
						blocked: e.blockedURI,
						target,
						page: location.pathname
					});
				});
			});
			await use(found);
			const unexpected = found.filter(
				(v) =>
					!v.page.startsWith('/api/') &&
					!(v.directive === 'style-src-attr' && KNOWN_INLINE_STYLE_TARGETS.has(v.target))
			);
			expect(unexpected, 'Content-Security-Policy violations').toEqual([]);
		},
		{ auto: true }
	]
});

export { expect };

export async function signIn(page: Page): Promise<void> {
	const resp = await page.request.post('/api/auth/login', {
		data: { password: PASSWORD },
		headers: { 'X-Requested-With': 'tagmail' }
	});
	expect(resp.ok()).toBe(true);
}

export function list(page: Page, name = 'Inbox') {
	return page.getByRole('listbox', { name });
}

export function row(page: Page, index: number) {
	return page.locator(`[role="option"][data-index="${index}"]`);
}

export async function openInbox(page: Page): Promise<void> {
	await signIn(page);
	await page.goto('/inbox');
	await expect(row(page, 0).locator('[data-loaded]')).toBeVisible();
}
