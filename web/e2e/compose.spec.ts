import type { Page } from '@playwright/test';
import { ME, XSS_INDEX, idAt, messagesOf, rowAt } from './mock/fixture.ts';
import { expect, openInbox, signIn, test } from './helpers.ts';

declare global {
	interface Window {
		__pwned?: string;
	}
}

interface MockState {
	drafts: { id: number; subject: string; html?: string; to: { email: string }[]; saves: number }[];
	sent: { subject: string; html?: string; text?: string; attachmentIds: string[] }[];
}

async function state(page: Page): Promise<MockState> {
	return (await (await page.request.get('/__mock/state')).json()) as MockState;
}

function compose(page: Page) {
	return page.locator('section.window');
}

function toast(page: Page, text: string | RegExp) {
	return page.locator('[data-toast]').filter({ hasText: text });
}

/** First thread whose newest message has a Cc, so reply-all has something to keep. */
function threadWithCc(): number {
	for (let i = 0; i < 2000; i++) {
		const last = messagesOf(rowAt(i)).at(-1)!;
		if (last.cc.length > 0 && last.from && last.from.email !== ME.email) return i;
	}
	throw new Error('fixture has no thread with a Cc');
}

test('c composes, autosaves, sends and reports the outbox status', async ({ page }) => {
	await openInbox(page);
	await page.keyboard.press('c');
	const win = compose(page);
	await expect(win).toBeVisible();
	const to = win.getByLabel('To recipients');
	await expect(to).toBeFocused();
	await to.fill('Ana Sousa <ana@sousa.family>, not-an-address');
	await to.press('Enter');
	await expect(win.locator('.chip')).toHaveCount(2);
	await expect(win.locator('.chip.bad')).toHaveText(
		/^not-an-address\s*\(not a valid address\)\s*$/
	);
	await win.getByLabel('Subject').fill('Lisbon photos');
	await win.getByRole('textbox', { name: 'Message body' }).click();
	await page.keyboard.type('Here are the best ones.');
	await win.getByRole('button', { name: 'Send' }).click();
	await expect(win.getByRole('alert')).toHaveText('Check the address “not-an-address”');
	await win.getByRole('button', { name: 'Remove not-an-address' }).click();

	await win.getByRole('button', { name: 'Bold' }).click();
	await page.keyboard.type(' Really.');
	await expect(win.getByRole('status')).toHaveText('Draft saved');
	const saved = await state(page);
	expect(saved.drafts).toHaveLength(1);
	expect(saved.drafts[0]).toMatchObject({
		subject: 'Lisbon photos',
		to: [{ email: 'ana@sousa.family' }]
	});
	expect(saved.drafts[0].html).toContain('<b> Really.</b>');

	await page.keyboard.press('Control+Enter');
	await expect(win).toHaveCount(0);
	await expect(toast(page, 'Sending…')).toBeVisible();
	await expect(toast(page, 'Message sent')).toBeVisible();
	const after = await state(page);
	expect(after.sent.map((d) => d.subject)).toEqual(['Lisbon photos']);
	expect(after.sent[0].text).toContain('Here are the best ones.');
});

test('a failed send says so and can be retried', async ({ page }) => {
	await page.request.post('/__mock/send-mode', { data: { mode: 'fail' } });
	await openInbox(page);
	await page.getByRole('button', { name: 'Compose' }).click();
	const win = compose(page);
	await win.getByLabel('To recipients').fill('lea.meier@meier-partner.ch');
	await win.getByLabel('Subject').fill('Contract');
	await win.getByRole('button', { name: 'Send' }).click();
	const failure = toast(page, "Couldn't send “Contract”");
	await expect(failure).toBeVisible();
	await page.getByRole('button', { name: '1 unsynced' }).click();
	const panel = page.getByRole('dialog', { name: 'Unsynced changes' });
	await expect(panel).toContainText('SMTP 554');
	await page.request.post('/__mock/send-mode', { data: { mode: 'ok' } });
	await panel.getByRole('button', { name: 'Retry' }).click();
	await expect(panel.getByText('Everything is in sync')).toBeVisible();
	await page.keyboard.press('Escape');
	await expect(toast(page, 'Message sent')).toBeVisible();
	expect((await state(page)).sent.map((d) => d.subject)).toEqual(['Contract']);
});

test('reply all keeps everyone but me; reply and forward prefill too', async ({ page }) => {
	const i = threadWithCc();
	const last = messagesOf(rowAt(i)).at(-1)!;
	await signIn(page);
	await page.goto(`/thread/${idAt(i)}`);
	await expect(page.getByRole('heading', { level: 1 })).toBeVisible();

	await page.keyboard.press('a');
	const reply = page.getByRole('region', { name: 'Reply' });
	const toChips = reply.getByRole('group', { name: 'To' }).locator('.chip');
	const ccChips = reply.getByRole('group', { name: 'Cc' }).locator('.chip');
	const names = (list: typeof last.to) =>
		list.filter((a) => a.email !== ME.email).map((a) => a.name ?? a.email);
	await expect(toChips).toHaveText([last.from!.name ?? last.from!.email, ...names(last.to)]);
	await expect(ccChips).toHaveText(names(last.cc));
	await expect(reply.getByText(ME.name!, { exact: true })).toHaveCount(0);
	await expect(reply.getByRole('textbox', { name: 'Message body' })).toContainText('wrote:');
	await reply.getByRole('button', { name: 'Discard draft' }).click();

	await page.keyboard.press('r');
	await expect(reply.getByRole('group', { name: 'To' }).locator('.chip')).toHaveText([
		last.from!.name ?? last.from!.email
	]);
	await expect(reply.getByRole('group', { name: 'Cc' })).toHaveCount(0);
	await expect(reply.getByRole('textbox', { name: 'Message body' })).toBeFocused();
	await page.keyboard.press('Control+Enter');
	await expect(toast(page, 'Message sent')).toBeVisible();

	await page.keyboard.press('f');
	await expect(reply.getByLabel('Subject')).toHaveValue(/^Fwd: /);
	await expect(reply.getByRole('group', { name: 'To' }).locator('.chip')).toHaveCount(0);
});

test('attachments upload with progress and the size cap is enforced', async ({ page }) => {
	await openInbox(page);
	await page.keyboard.press('c');
	const win = compose(page);
	const input = win.locator('input[type="file"]');
	await input.setInputFiles({
		name: 'notes.txt',
		mimeType: 'text/plain',
		buffer: Buffer.from('meeting notes\n'.repeat(1000))
	});
	const files = win.getByRole('list', { name: 'Attachments' });
	await expect(files.getByRole('listitem').filter({ hasText: 'notes.txt' })).toContainText('14 KB');
	await expect(files.locator('progress')).toHaveCount(0);

	await input.setInputFiles({
		name: 'scan.tiff',
		mimeType: 'image/tiff',
		buffer: Buffer.alloc(26 * 1024 * 1024)
	});
	const big = files.getByRole('listitem').filter({ hasText: 'scan.tiff' });
	await expect(big.getByRole('alert')).toHaveText('Larger than 25 MB');

	const dataTransfer = await page.evaluateHandle(() => {
		const dt = new DataTransfer();
		dt.items.add(new File(['%PDF-1.4'], 'dropped.pdf', { type: 'application/pdf' }));
		return dt;
	});
	const form = win.locator('form');
	await form.dispatchEvent('dragenter', { dataTransfer });
	await expect(win.getByText('Drop files to attach')).toBeVisible();
	await form.dispatchEvent('drop', { dataTransfer });
	await expect(files.getByRole('listitem').filter({ hasText: 'dropped.pdf' })).toBeVisible();
	await expect(win.getByText('Drop files to attach')).toHaveCount(0);

	await win.getByLabel('To recipients').fill('kenji@watanabe.jp');
	await win.getByLabel('Subject').fill('Files');
	await win.getByRole('button', { name: 'Send' }).click();
	await expect(toast(page, 'Message sent')).toBeVisible();
	expect((await state(page)).sent[0].attachmentIds).toHaveLength(2);
});

test('hostile HTML in a quote or a paste never runs in the composer', async ({ page }) => {
	await signIn(page);
	await page.goto(`/thread/${idAt(XSS_INDEX)}`);
	await expect(page.getByRole('heading', { level: 1 })).toBeVisible();
	await page.keyboard.press('r');
	const body = page.getByRole('region', { name: 'Reply' }).getByRole('textbox', {
		name: 'Message body'
	});
	await expect(body).toContainText('Nothing to see here.');
	await body.evaluate((el) => {
		const dt = new DataTransfer();
		dt.setData(
			'text/html',
			'<p>pasted</p><img src=x onerror="window.__pwned=\'paste-img\'"><script>window.__pwned=\'paste-script\'</script><a href="javascript:window.__pwned=\'paste-link\'">x</a><p style="color:red" onclick="window.__pwned=\'click\'">styled</p>'
		);
		dt.setData('text/plain', 'pasted');
		el.focus();
		el.dispatchEvent(
			new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true })
		);
	});
	await expect(body).toContainText('styled');
	await body.getByText('styled').click();
	await page.waitForTimeout(200);
	expect(await page.evaluate(() => window.__pwned)).toBeUndefined();
	const html = await body.innerHTML();
	expect(html).not.toMatch(/<(script|img|svg|iframe|object|form|style)\b/i);
	expect(html).not.toMatch(/\son\w+=|javascript:|style=/i);
});
