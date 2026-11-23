import { describe, expect, it, vi } from 'vitest';
import type { OutboxItemDto } from '../transport';
import { Outbox, type OutboxApi } from './outbox.svelte';

function item(patch: Partial<OutboxItemDto>): OutboxItemDto {
	return {
		id: 7,
		opId: 'send-1',
		accountId: 1,
		draftId: 3,
		subject: 'Hi',
		to: [],
		status: 'queued',
		attempts: 0,
		error: null,
		createdAt: 1,
		sentAt: null,
		...patch
	};
}

function setup(rows: OutboxItemDto[]) {
	const shown: { text: string; actions: string[] }[] = [];
	const retryOutbox = vi.fn((id: number) => Promise.resolve(item({ id, status: 'queued' })));
	const api: OutboxApi = {
		send: (body: { opId: string }) => Promise.resolve({ opId: body.opId, status: 'queued' }),
		outbox: () => Promise.resolve(rows),
		cancelOutbox: (id: number) => Promise.resolve(item({ id })),
		retryOutbox
	};
	const outbox = new Outbox(
		api,
		{
			show: (text, o = {}) => shown.push({ text, actions: (o.actions ?? []).map((a) => a.label) }),
			dismissKey: () => {}
		},
		() => {},
		() => 'send-1'
	);
	const event = (kind: 'upsert' | 'delete', payload: unknown) =>
		outbox.changes({
			changes: [{ seq: 1, entity: 'outbox', id: 7, op: { kind }, payload }],
			next: 'e:1',
			more: false
		});
	return { outbox, retryOutbox, shown, event };
}

describe('Outbox', () => {
	it('learns the item id after sending and follows its events to sent', async () => {
		const { outbox, shown, event } = setup([item({})]);
		await outbox.send(3, 'Hi');
		expect(outbox.items[0]).toMatchObject({ id: 7, status: 'queued' });
		expect(shown.at(-1)).toEqual({ text: 'Sending…', actions: ['Cancel'] });
		event('upsert', { status: 'sending' });
		event('upsert', { status: 'sent' });
		expect(outbox.items).toEqual([]);
		expect(shown.at(-1)?.text).toBe('Message sent');
	});

	it('offers Retry on failure and retries through the outbox endpoint', async () => {
		const { outbox, retryOutbox, shown, event } = setup([item({})]);
		await outbox.send(3, 'Hi');
		event('upsert', { status: 'failed', error: 'SMTP 554' });
		expect(outbox.failed).toMatchObject([{ error: 'SMTP 554' }]);
		expect(shown.at(-1)).toEqual({
			text: "Couldn't send “Hi”",
			actions: ['Retry']
		});
		await outbox.retry(outbox.failed[0]);
		expect(retryOutbox).toHaveBeenCalledWith(7);
		expect(outbox.failed).toEqual([]);
	});

	it('restores unsent items from an earlier visit', async () => {
		const { outbox } = setup([
			item({ status: 'failed', error: 'x' }),
			item({ id: 8, opId: 'o', status: 'sent' })
		]);
		await outbox.load();
		expect(outbox.items.map((i) => i.id)).toEqual([7]);
	});
});
