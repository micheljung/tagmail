import type { ApiEvent, ApiTransport, ChangeDto, ThreadRowDto } from '@tagmail/api-types';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { encodeIdIndex } from '../../../e2e/mock/idIndex';
import { Api, CURSOR_HEADER } from '../transport/api';
import { EventHub } from './events.svelte';
import { NavStore } from './nav.svelte';
import { REFETCH_DEBOUNCE_MS, ThreadList } from './threadList.svelte';

const EPOCH = '00000000000000aa';
const INBOX = 1;
const ARCHIVE = 2;

function row(id: number, lastDate: number): ThreadRowDto {
	return {
		id,
		accountId: 1,
		subject: `s${id}`,
		snippet: '',
		lastDate,
		messageCount: 1,
		unreadCount: 0,
		starred: false,
		hasAttachment: false,
		senders: [],
		labels: [],
		mailboxIds: [1]
	};
}

function fixture() {
	const dates = new Map<number, number>([
		[50, 500],
		[40, 400],
		[30, 300],
		[20, 200],
		[10, 100]
	]);
	let order = [50, 40, 30, 20, 10];
	let seq = 10;
	const indexCalls: string[] = [];
	let push: (e: ApiEvent) => void = () => {};
	const transport: ApiTransport = {
		request(path) {
			if (path.startsWith('/api/views/')) {
				indexCalls.push(path);
				return Promise.resolve(
					new Response(encodeIdIndex(order), { headers: { [CURSOR_HEADER]: `${EPOCH}:${seq}` } })
				);
			}
			if (path.startsWith('/api/threads?ids=')) {
				const ids = path.slice('/api/threads?ids='.length).split(',').map(Number);
				const threads = ids.filter((id) => dates.has(id)).map((id) => row(id, dates.get(id)!));
				return Promise.resolve(Response.json({ cursor: `${EPOCH}:${seq}`, threads }));
			}
			if (path === '/api/mailboxes') {
				return Promise.resolve(
					Response.json([
						{ id: INBOX, accountId: 1, name: 'INBOX', delimiter: '/', specialUse: null },
						{ id: ARCHIVE, accountId: 1, name: 'Archive', delimiter: '/', specialUse: 'archive' }
					])
				);
			}
			if (path === '/api/labels') return Promise.resolve(Response.json([]));
			return Promise.resolve(new Response('{}', { status: 404 }));
		},
		events(_since, onEvent) {
			push = onEvent;
			return () => {};
		}
	};
	const api = new Api(transport);
	const hub = new EventHub(transport);
	const nav = new NavStore(api);
	const list = new ThreadList('inbox', api, hub, nav);
	const send = (changes: Omit<ChangeDto, 'seq'>[]) => {
		const page = changes.map((c) => ({ ...c, seq: ++seq }));
		push({
			type: 'changes',
			id: `${EPOCH}:${seq}`,
			data: { changes: page, next: `${EPOCH}:${seq}`, more: false }
		});
	};
	return {
		list,
		nav,
		send,
		indexCalls,
		setOrder: (o: number[]) => (order = o),
		setDate: (id: number, d: number) => dates.set(id, d),
		ids: () => list.index.slice(0, list.count),
		push: (e: ApiEvent) => push(e)
	};
}

const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((resolve) => setImmediate(resolve));
};

async function ready() {
	const f = fixture();
	await f.nav.load();
	f.list.attached = true;
	await f.list.load();
	f.list.ensureWindow({ first: 0, last: 4 });
	await flush();
	return f;
}

const upsert = (id: number, lastDate: number, mailboxes: number[]) => ({
	entity: 'thread' as const,
	id,
	op: { kind: 'upsert' as const },
	payload: { lastDate, mailboxes, removedFrom: [] }
});

describe('ThreadList', () => {
	beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] }));
	afterEach(() => vi.useRealTimers());

	it('loads the index and keeps the selection on the same thread across patches', async () => {
		const f = await ready();
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
		f.list.select(2);
		expect(f.list.selectedId).toBe(30);
		f.setDate(60, 600);
		f.send([upsert(60, 600, [INBOX])]);
		expect(f.ids()).toEqual([60, 50, 40, 30, 20, 10]);
		expect(f.list.selectedId).toBe(30);
		expect(f.list.activeIndex).toBe(3);
	});

	it('moves a thread by its new last date and drops threads that left the view', async () => {
		const f = await ready();
		f.send([upsert(10, 350, [INBOX])]);
		expect(f.ids()).toEqual([50, 40, 10, 30, 20]);
		f.send([upsert(40, 400, [ARCHIVE])]);
		expect(f.ids()).toEqual([50, 10, 30, 20]);
	});

	it('removes deleted and merged threads and moves the selection to the next row', async () => {
		const f = await ready();
		f.list.select(1);
		f.send([{ entity: 'thread', id: 40, op: { kind: 'delete' }, payload: null }]);
		expect(f.ids()).toEqual([50, 30, 20, 10]);
		expect(f.list.selectedId).toBe(30);
		f.send([{ entity: 'thread', id: 20, op: { kind: 'merged', target: 30 }, payload: null }]);
		expect(f.ids()).toEqual([50, 30, 10]);
	});

	it('refetches the index when the spot falls in an unloaded gap', async () => {
		const f = await ready();
		f.list.rows.clear();
		f.send([upsert(45, 450, [INBOX])]);
		expect(f.indexCalls).toHaveLength(1);
		f.setOrder([50, 45, 40, 30, 20, 10]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		await flush();
		expect(f.indexCalls).toHaveLength(2);
		expect(f.ids()).toEqual([50, 45, 40, 30, 20, 10]);
	});

	it('keeps the selection on a thread that changed in place', async () => {
		const f = await ready();
		f.list.select(1);
		f.send([upsert(40, 400, [INBOX])]);
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
		expect(f.list.selectedId).toBe(40);
		expect(f.list.rows.peek(40)).toBeDefined();
	});

	it('takes threads out, puts them back and ticks ranges', async () => {
		const f = await ready();
		f.list.select(1);
		f.list.toggleChecked(1);
		f.list.toggleChecked(3, true);
		expect(f.list.targets()).toEqual([40, 30, 20]);
		const removed = f.list.takeOut([40, 20]);
		expect(removed).toEqual([
			{ id: 40, at: 1 },
			{ id: 20, at: 3 }
		]);
		expect(f.ids()).toEqual([50, 30, 10]);
		expect(f.list.selectedId).toBe(30);
		expect(f.list.targets()).toEqual([30]);
		f.list.putBack(removed);
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
		expect(f.list.selectedId).toBe(40);
	});

	it('refetches after bulk changes', async () => {
		const f = await ready();
		f.send([{ entity: 'mailbox', id: INBOX, op: { kind: 'bulk', mailbox: INBOX }, payload: null }]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.indexCalls).toHaveLength(2);
	});

	it('skips changes the loaded index already contains', async () => {
		const f = await ready();
		f.push({
			type: 'changes',
			id: `${EPOCH}:10`,
			data: {
				changes: [{ seq: 9, entity: 'thread', id: 50, op: { kind: 'delete' }, payload: null }],
				next: `${EPOCH}:10`,
				more: false
			}
		});
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
	});

	it('reloads everything on reset', async () => {
		const f = await ready();
		f.setOrder([10]);
		f.push({ type: 'reset', data: { error: 'gone', message: 'reload' } });
		await flush();
		expect(f.ids()).toEqual([10]);
	});
});
