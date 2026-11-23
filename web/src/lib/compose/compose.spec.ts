import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ATTACHMENT_MAX_BYTES, DRAFT_ATTACHMENTS_MAX_BYTES } from '../transport';
import { parseAddresses } from './addresses';
import { stripStyles } from './sanitize';
import { AUTOSAVE_MS, Draft, type DraftApi } from './draft.svelte';

describe('parseAddresses', () => {
	it('splits on separators outside quotes and brackets', () => {
		expect(parseAddresses('"Doe, Jane" <jane@doe.org>; bob@x.io\nmailto:c@d.ch')).toEqual([
			{ name: 'Doe, Jane', email: 'jane@doe.org', valid: true },
			{ name: null, email: 'bob@x.io', valid: true },
			{ name: null, email: 'c@d.ch', valid: true }
		]);
	});

	it('flags addresses that cannot be delivered', () => {
		expect(parseAddresses('nobody, a@b, a b@c.de, ok@mail.example').map((r) => r.valid)).toEqual([
			false,
			false,
			false,
			true
		]);
	});
});

function fakeApi(): DraftApi & { calls: string[]; uploads: ReturnType<typeof vi.fn> } {
	const calls: string[] = [];
	const uploads = vi.fn((file: File) =>
		Promise.resolve({ id: 'a1', name: file.name, size: file.size, contentType: 'x' })
	);
	return {
		calls,
		uploads,
		createDraft: (body, keepalive) => {
			calls.push(`create${body.saveNow ? '+now' : ''}${keepalive ? '+keepalive' : ''}`);
			return Promise.resolve({ id: 9 });
		},
		updateDraft: (_id, body, keepalive) => {
			calls.push(`update${body.saveNow ? '+now' : ''}${keepalive ? '+keepalive' : ''}`);
			return Promise.resolve();
		},
		deleteDraft: vi.fn(() => Promise.resolve()),
		uploadAttachment: (file) => uploads(file),
		removeAttachment: vi.fn(() => Promise.resolve())
	};
}

function file(name: string, size: number): File {
	const f = new File([], name);
	Object.defineProperty(f, 'size', { value: size });
	return f;
}

describe('Draft', () => {
	beforeEach(() => vi.useFakeTimers());
	afterEach(() => vi.useRealTimers());

	it('autosaves once typing pauses: create first, then replace', async () => {
		const api = fakeApi();
		const d = new Draft(api, { identityId: 1 });
		d.subject = 'Hi';
		d.touch();
		await vi.advanceTimersByTimeAsync(AUTOSAVE_MS - 100);
		d.touch();
		await vi.advanceTimersByTimeAsync(AUTOSAVE_MS - 100);
		expect(api.calls).toEqual([]);
		await vi.advanceTimersByTimeAsync(200);
		expect(api.calls).toEqual(['create']);
		expect(d.draftId).toBe(9);
		d.subject = 'Hi there';
		d.touch();
		await vi.advanceTimersByTimeAsync(AUTOSAVE_MS);
		expect(api.calls).toEqual(['create', 'update']);
		await d.save();
		expect(api.calls).toEqual(['create', 'update']);
	});

	it('asks for the Drafts mailbox only when closing or when the page hides', async () => {
		const api = fakeApi();
		const d = new Draft(api, { identityId: 1 });
		d.subject = 'Hi';
		d.touch();
		await vi.advanceTimersByTimeAsync(AUTOSAVE_MS);
		d.flush();
		await vi.advanceTimersByTimeAsync(0);
		d.flush();
		await vi.advanceTimersByTimeAsync(0);
		d.subject = 'Hi again';
		d.touch();
		await d.close();
		await d.close();
		expect(api.calls).toEqual(['create', 'update+now+keepalive', 'update+now']);
	});

	it('never stores an untouched empty draft', async () => {
		const api = fakeApi();
		await new Draft(api, { identityId: 1 }).save();
		expect(api.calls).toEqual([]);
	});

	it('blocks sending without valid recipients', () => {
		const d = new Draft(fakeApi(), { identityId: 1 });
		expect(d.sendBlocker).toBe('Add at least one recipient');
		d.addRecipients('to', 'nope');
		expect(d.sendBlocker).toBe('Check the address “nope”');
		d.removeRecipient('to', 0);
		d.addRecipients('cc', 'ok@mail.example, ok@mail.example');
		expect(d.cc).toHaveLength(1);
		expect(d.sendBlocker).toBeNull();
	});

	it('refuses files over the per-file and per-draft caps before uploading', async () => {
		const api = fakeApi();
		const d = new Draft(api, { identityId: 1 });
		d.attach([file('huge.bin', ATTACHMENT_MAX_BYTES + 1)]);
		expect(d.uploads[0]).toMatchObject({ status: 'error', error: 'Larger than 25 MB' });
		const chunk = 20 * 1024 * 1024;
		d.attach([file('a.bin', chunk), file('b.bin', chunk)]);
		expect(DRAFT_ATTACHMENTS_MAX_BYTES).toBeLessThan(2 * chunk);
		expect(d.uploads.map((u) => u.status)).toEqual(['error', 'uploading', 'error']);
		expect(d.uploads[2].error).toBe('Attachments would exceed 35 MB in total');
		await vi.advanceTimersByTimeAsync(0);
		expect(api.uploads).toHaveBeenCalledTimes(1);
		expect(d.uploads[1]).toMatchObject({ status: 'done', id: 'a1' });
	});
});

describe('stripStyles', () => {
	it('cuts style elements and attributes in every quoting style', () => {
		expect(
			stripStyles(
				`<style>p{}</style><p style="a" STYLE='b' style=c data-x="style=d">x</p><style>unterminated`
			)
		).toBe('<p data-x="style=d">x</p>');
	});
});
