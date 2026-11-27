import type { ApiEvent, ApiTransport, ChangeDto, ThreadRowDto } from '@tagmail/api-types';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { encodeIdIndex } from '../../../e2e/mock/idIndex';
import { OpsStore } from '../ops/ops.svelte';
import { LIST_PATH_KEY } from '../readingPane';
import type { NewOpDto, OpDto } from '../transport';
import { Api, CURSOR_HEADER, type ViewId } from '../transport/api';
import { EventHub } from './events.svelte';
import { NavStore } from './nav.svelte';
import {
	REFETCH_DEBOUNCE_MS,
	REFRESH_MIN_SPIN_MS,
	REFRESH_TIMEOUT_MS,
	ThreadList,
	ThreadLists,
	UP_TO_DATE_MS
} from './threadList.svelte';

const EPOCH = '00000000000000aa';
const INBOX = 1;
const ARCHIVE = 2;
const UNREAD_VIEW = 'search:in:inbox is:unread';

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

interface Posted {
	request: NewOpDto;
	resolve: (ack: OpDto) => void;
	reject: (e: unknown) => void;
}

function ack(request: NewOpDto, status: OpDto['status']): OpDto {
	return {
		opId: request.opId,
		seq: 1,
		accountId: 1,
		kind: request.kind,
		status,
		error: status === 'failed' ? 'refused' : null,
		attempts: 0,
		threadIds: request.threadIds ?? [],
		label: null,
		mailboxId: null,
		createdAt: 1
	};
}

function fixture(view: ViewId = 'inbox', options: { unreadFirst?: boolean } = {}) {
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
	const unread = { ids: new Set([40, 20]), seq: null as number | null, fail: false, hold: false };
	const unreadCalls: (() => void)[] = [];
	let unreadCount = 0;
	const syncCalls: { path: string; method: string | undefined }[] = [];
	let sync: () => Promise<Response> = () => Promise.resolve(new Response(null, { status: 202 }));
	const lastErrors: Record<number, string | null> = {};
	let statusCalls = 0;
	let push: (e: ApiEvent) => void = () => {};
	const transport: ApiTransport = {
		request(path, init) {
			if (path.startsWith('/api/views/') && path.endsWith('/sync')) {
				syncCalls.push({ path, method: init?.method });
				return sync();
			}
			if (path === '/api/status') {
				statusCalls++;
				const accounts = [1, 2].map((id) => ({
					id,
					name: `a${id}`,
					email: null,
					status: 'running',
					phase: 'live',
					mailboxesTotal: 1,
					mailboxesLive: 1,
					textPending: 0,
					lastError: lastErrors[id] ?? null,
					capabilities: [],
					degraded: []
				}));
				return Promise.resolve(Response.json({ accounts }));
			}
			if (path.startsWith('/api/views/') && decodeURIComponent(path).includes(UNREAD_VIEW)) {
				unreadCount++;
				return new Promise((resolve) => {
					const answer = () =>
						resolve(
							unread.fail
								? Response.json({ error: 'busy', message: 'search down' }, { status: 500 })
								: new Response(encodeIdIndex(order.filter((id) => unread.ids.has(id))), {
										headers: { [CURSOR_HEADER]: `${EPOCH}:${unread.seq ?? seq}` }
									})
						);
					if (unread.hold) unreadCalls.push(answer);
					else answer();
				});
			}
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
	const posted: Posted[] = [];
	let n = 0;
	// eslint-disable-next-line prefer-const -- the list and the store refer to each other
	let ops: OpsStore;
	const list = new ThreadList(view, api, hub, nav, {
		unreadFirst: options.unreadFirst,
		unreadIntents: () => ops.unreadIntents(),
		touches: (id) => ops.touches(id)
	});
	ops = new OpsStore({
		api: {
			postOp: (request) =>
				new Promise((resolve, reject) => posted.push({ request, resolve, reject })),
			retryOp: () => Promise.reject(new Error('unused')),
			discardOp: () => Promise.resolve(),
			ops: () => Promise.resolve([])
		},
		lists: () => [list],
		context: () => ({ inboxIds: new Set([INBOX]) }),
		names: () => ({ label: (k) => k, mailbox: () => 'Box' }),
		notify: { show: () => 0, dismissKey() {} },
		uuid: () => `op-${++n}`
	});
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
		ops,
		posted,
		unread,
		/** Answers the held unread searches. */
		answerUnread: () => unreadCalls.splice(0).forEach((answer) => answer()),
		unreadCount: () => unreadCount,
		send,
		indexCalls,
		syncCalls,
		statusCalls: () => statusCalls,
		setSync: (s: () => Promise<Response>) => (sync = s),
		setLastError: (id: number, e: string | null) => (lastErrors[id] = e),
		setOrder: (o: number[]) => (order = o),
		setDate: (id: number, d: number) => dates.set(id, d),
		ids: () => list.index.slice(0, list.count),
		push: (e: ApiEvent) => push(e)
	};
}

const flush = async () => {
	for (let i = 0; i < 4; i++) await new Promise((resolve) => setImmediate(resolve));
};

async function ready(view?: ViewId, options?: { unreadFirst?: boolean }) {
	const f = fixture(view, options);
	await f.nav.load();
	f.list.attach();
	await f.list.load();
	f.list.ensureWindow({ first: 0, last: 4 });
	await flush();
	return f;
}

const upsert = (id: number, lastDate: number, mailboxes: number[], accountId?: number) => ({
	entity: 'thread' as const,
	id,
	op: { kind: 'upsert' as const },
	payload: { accountId, lastDate, mailboxes, removedFrom: [] }
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

	it('lets only its own account into a scoped list', async () => {
		const f = await ready('account:2/inbox');
		f.send([upsert(60, 600, [INBOX], 1)]);
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
		f.send([upsert(40, 400, [INBOX], 1)]);
		expect(f.ids()).toEqual([50, 30, 20, 10]);
		f.send([upsert(70, 700, [INBOX], 2)]);
		expect(f.ids()).toEqual([70, 50, 30, 20, 10]);
		f.send([upsert(80, 800, [INBOX])]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.indexCalls).toHaveLength(2);
		expect(f.indexCalls[0]).toBe(`/api/views/${encodeURIComponent('account:2/inbox')}/index`);
	});

	it('reloads everything on reset', async () => {
		const f = await ready();
		f.setOrder([10]);
		f.push({ type: 'reset', data: { error: 'gone', message: 'reload' } });
		await flush();
		expect(f.ids()).toEqual([10]);
	});
});

describe('ThreadList, unread first', () => {
	beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] }));
	afterEach(() => {
		vi.useRealTimers();
		vi.unstubAllGlobals();
	});

	const sectioned = () => ready('inbox', { unreadFirst: true });

	/** A change that makes the list ask the unread search again. */
	const touch = (threadId: number) => ({
		entity: 'message' as const,
		id: 900 + threadId,
		op: { kind: 'upsert' as const },
		payload: { threadId }
	});

	async function refresh(f: ReturnType<typeof fixture>) {
		f.send([touch(50)]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		await flush();
	}

	async function settle(f: ReturnType<typeof fixture>, i: number, status: OpDto['status']) {
		f.posted[i].resolve(ack(f.posted[i].request, status));
		await flush();
	}

	it('shows unread threads first, each section newest first', async () => {
		const f = await sectioned();
		expect(f.list.sectioned).toBe(true);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		expect(f.list.split).toBe(2);
		expect(f.list.selectedId).toBe(40);
	});

	it('stays flat outside the inbox and without the option', async () => {
		const label = await ready('label:work', { unreadFirst: true });
		expect(label.list.sectioned).toBe(false);
		expect(label.unreadCount()).toBe(0);
		const flat = await ready();
		expect(flat.ids()).toEqual([50, 40, 30, 20, 10]);
		expect(flat.unreadCount()).toBe(0);
	});

	it('projects read/unread ops: unread moves at once, read is held until released', async () => {
		const f = await sectioned();
		f.ops.run({ kind: 'markUnread', threadIds: [30] }, { view: 'inbox' });
		expect(f.ids()).toEqual([40, 30, 20, 50, 10]);
		f.ops.run({ kind: 'markRead', threadIds: [20] }, { view: 'inbox' });
		expect(f.list.member(20)).toBe(false);
		expect(f.ids()).toEqual([40, 30, 20, 50, 10]);
		f.list.releaseHeld();
		expect(f.ids()).toEqual([40, 30, 50, 20, 10]);
		expect(f.list.split).toBe(2);
	});

	it('waits for the unread set on a cold load', async () => {
		const f = fixture('inbox', { unreadFirst: true });
		await f.nav.load();
		f.unread.hold = true;
		const loaded = f.list.load();
		await flush();
		expect(f.list.status).toBe('loading');
		expect(f.list.count).toBe(0);
		f.answerUnread();
		await loaded;
		expect(f.list.status).toBe('ready');
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('falls back to a flat list when the first unread search fails', async () => {
		const f = fixture('inbox', { unreadFirst: true });
		await f.nav.load();
		f.unread.fail = true;
		f.list.attach();
		await f.list.load();
		expect(f.list.sectioned).toBe(false);
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
		expect(f.list.error).toBe('search down');
		f.unread.fail = false;
		await refresh(f);
		expect(f.list.sectioned).toBe(true);
		expect(f.list.error).toBeNull();
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		f.unread.fail = true;
		f.unread.ids = new Set();
		await refresh(f);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('keeps a pending markRead across an unread refresh', async () => {
		const f = await sectioned();
		f.ops.run({ kind: 'markRead', threadIds: [40] }, { view: 'inbox' });
		f.list.releaseHeld();
		await refresh(f);
		expect(f.unreadCount()).toBe(2);
		expect(f.list.member(40)).toBe(false);
		expect(f.ids()).toEqual([20, 50, 40, 30, 10]);
	});

	it('does not bounce between the ack and the next unread answer', async () => {
		const f = await sectioned();
		f.ops.run({ kind: 'markRead', threadIds: [40] }, { view: 'inbox' });
		f.list.releaseHeld();
		expect(f.ids()).toEqual([20, 50, 40, 30, 10]);
		f.unread.hold = true;
		await settle(f, 0, 'done');
		expect(f.ops.unreadIntents()).toEqual([]);
		expect(f.list.baseUnread(40)).toBe(false);
		expect(f.ids()).toEqual([20, 50, 40, 30, 10]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.ids()).toEqual([20, 50, 40, 30, 10]);
		f.unread.ids.delete(40);
		f.answerUnread();
		await flush();
		expect(f.ids()).toEqual([20, 50, 40, 30, 10]);
	});

	it('undoes a confirmed markRead', async () => {
		const f = await sectioned();
		const op = f.ops.run({ kind: 'markRead', threadIds: [40] }, { view: 'inbox' });
		f.list.releaseHeld();
		await settle(f, 0, 'done');
		f.ops.undo(op);
		expect(f.list.member(40)).toBe(true);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('moves a row on a refresh without loading it or the index', async () => {
		const f = await sectioned();
		f.list.rows.clear();
		f.unread.ids = new Set([10]);
		await refresh(f);
		f.list.releaseHeld();
		expect(f.ids()).toEqual([10, 50, 40, 30, 20]);
		expect(f.indexCalls).toHaveLength(1);
	});

	it('puts archived threads back into their sections', async () => {
		const f = await sectioned();
		const op = f.ops.run({ kind: 'archive', threadIds: [40, 30] }, { view: 'inbox' });
		expect(f.ids()).toEqual([20, 50, 10]);
		expect(op.removed[0].entries).toEqual([
			{ id: 40, at: 1, wasUnread: true },
			{ id: 30, at: 2, wasUnread: false }
		]);
		await settle(f, 0, 'done');
		// Reloaded in between: the server no longer has them in the inbox.
		f.setOrder([50, 20, 10]);
		await f.list.load();
		expect(f.ids()).toEqual([20, 50, 10]);
		f.ops.undo(op);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('restores the base unread state under a pending markRead', async () => {
		const f = await sectioned();
		f.ops.run({ kind: 'markRead', threadIds: [40] }, { view: 'inbox' });
		const archive = f.ops.run({ kind: 'archive', threadIds: [40] }, { view: 'inbox' });
		expect(archive.removed[0].entries).toEqual([{ id: 40, at: 1, wasUnread: true }]);
		f.ops.undo(archive);
		f.list.releaseHeld();
		expect(f.ids()).toEqual([20, 50, 40, 30, 10]);
		await settle(f, 0, 'failed');
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('drops overrides only for searches started after them', async () => {
		const f = await sectioned();
		const removed = f.list.takeOut([40]);
		f.unread.ids.delete(40);
		f.unread.hold = true;
		f.send([touch(50)]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.unreadCount()).toBe(2);
		f.list.putBack(removed);
		f.answerUnread();
		await flush();
		expect(f.list.baseUnread(40)).toBe(true);
		f.unread.hold = false;
		await refresh(f);
		expect(f.list.baseUnread(40)).toBe(false);
	});

	it('keeps an override while an op on the thread is still being sent', async () => {
		const f = await sectioned();
		const op = f.ops.run({ kind: 'archive', threadIds: [40] }, { view: 'inbox' });
		await settle(f, 0, 'done');
		f.unread.ids.delete(40);
		f.ops.undo(op);
		await refresh(f);
		expect(f.list.baseUnread(40)).toBe(true);
		await settle(f, 1, 'done');
		await refresh(f);
		expect(f.list.baseUnread(40)).toBe(false);
	});

	it('asks again when the unread answer lags the index cursor', async () => {
		const f = fixture('inbox', { unreadFirst: true });
		await f.nav.load();
		f.list.attach();
		f.unread.seq = 5;
		await f.list.load();
		expect(f.unreadCount()).toBe(1);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.unreadCount()).toBe(2);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.unreadCount()).toBe(2);
		// A change the index already holds still refreshes the unread set.
		f.push({
			type: 'changes',
			id: `${EPOCH}:10`,
			data: {
				changes: [{ seq: 9, entity: 'thread', id: 50, op: { kind: 'delete' }, payload: null }],
				next: `${EPOCH}:10`,
				more: false
			}
		});
		expect(f.ids()).toContain(50);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.unreadCount()).toBe(3);
	});

	it('shows new mail as unread', async () => {
		const f = await sectioned();
		f.setDate(60, 600);
		f.send([upsert(60, 600, [INBOX])]);
		expect(f.ids()).toEqual([60, 40, 20, 50, 30, 10]);
	});

	it('keeps a held row in place on a same-date upsert, without a refetch', async () => {
		const f = await sectioned();
		f.list.rows.clear();
		f.list.rows.ensure([40, 20]);
		await flush();
		const calls = f.indexCalls.length;
		f.ops.run({ kind: 'markRead', threadIds: [40] }, { view: 'inbox' });
		// The read flip arrives through the feed; the base's first row (50) is not loaded.
		f.send([upsert(40, 400, [INBOX])]);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		await flush();
		expect(f.indexCalls.length).toBe(calls);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('keeps a held row that is not cached in place on a same-date upsert', async () => {
		const f = await sectioned();
		f.ops.run({ kind: 'markRead', threadIds: [40] }, { view: 'inbox' });
		f.list.rows.clear();
		const calls = f.indexCalls.length;
		f.send([upsert(40, 400, [INBOX])]);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		await flush();
		expect(f.indexCalls.length).toBe(calls);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('counts attachments and lets held rows go when the last one ends', async () => {
		const f = fixture('inbox', { unreadFirst: true });
		await f.nav.load();
		const detachA = f.list.attach();
		const detachB = f.list.attach();
		await f.list.load();
		f.ops.run({ kind: 'markRead', threadIds: [40] }, { view: 'inbox' });
		detachA();
		detachA();
		expect(f.list.attached).toBe(true);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		detachB();
		expect(f.list.attached).toBe(false);
		expect(f.ids()).toEqual([20, 50, 40, 30, 10]);
	});

	it('refreshes a detached list only when it is attached again', async () => {
		const f = fixture('inbox', { unreadFirst: true });
		await f.nav.load();
		await f.list.load();
		f.list.ensureWindow({ first: 0, last: 4 });
		await flush();
		f.send([touch(50)]);
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.unreadCount()).toBe(1);
		const detach = f.list.attach();
		await vi.advanceTimersByTimeAsync(REFETCH_DEBOUNCE_MS);
		expect(f.unreadCount()).toBe(2);
		detach();
		expect(f.list.attached).toBe(false);
	});

	it('releases held rows when the window loses focus or the tab hides', async () => {
		const doc = Object.assign(new EventTarget(), {
			visibilityState: 'visible',
			hasFocus: (): boolean => true
		});
		vi.stubGlobal('document', doc);
		vi.stubGlobal('window', new EventTarget());
		const f = await sectioned();
		f.ops.run({ kind: 'markRead', threadIds: [40, 20] }, { view: 'inbox' });
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		// Focus went into a message frame: the document still has it.
		window.dispatchEvent(new Event('blur'));
		await vi.advanceTimersByTimeAsync(0);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		await refresh(f);
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
		doc.hasFocus = () => false;
		window.dispatchEvent(new Event('blur'));
		await vi.advanceTimersByTimeAsync(0);
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);

		f.ops.run({ kind: 'markUnread', threadIds: [30] }, { view: 'inbox' });
		f.ops.run({ kind: 'markRead', threadIds: [30] }, { view: 'inbox' });
		expect(f.ids()).toEqual([30, 50, 40, 20, 10]);
		doc.visibilityState = 'hidden';
		doc.dispatchEvent(new Event('visibilitychange'));
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
	});

	it('ignores ops once disposed', async () => {
		const f = await sectioned();
		f.list.dispose();
		expect(f.list.takeOut([40])).toEqual([]);
		f.list.putBack([{ id: 99, at: 0, wasUnread: true }]);
		f.list.opsChanged({ kind: 'markRead', threadIds: [40] }, 'activate');
		expect(f.ids()).toEqual([40, 20, 50, 30, 10]);
	});

	it('recreates a cached list when the inbox type changes', () => {
		const made: boolean[] = [];
		const f = fixture();
		const lists = new ThreadLists((_view, { unreadFirst }) => {
			made.push(unreadFirst);
			return f.list;
		});
		lists.open('inbox', '/inbox');
		lists.open('inbox', '/inbox');
		lists.open('inbox', '/inbox', { unreadFirst: true });
		expect(made).toEqual([false, true]);
	});

	it('remembers the opened list path for this tab', () => {
		const store = new Map<string, string>();
		vi.stubGlobal('sessionStorage', {
			getItem: (k: string) => store.get(k) ?? null,
			setItem: (k: string, v: string) => store.set(k, v)
		});
		try {
			const f = fixture();
			const lists = new ThreadLists(() => f.list);
			lists.open('label:work', '/label/work');
			expect(store.get(LIST_PATH_KEY)).toBe('/label/work');
		} finally {
			vi.unstubAllGlobals();
		}
	});
});

describe('ThreadList.refresh', () => {
	beforeEach(() => vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] }));
	afterEach(() => vi.useRealTimers());

	/** A list that started a refresh whose sync request already answered. */
	async function refreshing(view?: ViewId) {
		const f = await ready(view);
		void f.list.refresh();
		await flush();
		expect(f.list.refreshing).toBe('busy');
		return f;
	}

	const at = (ms: number) => vi.advanceTimersByTimeAsync(ms);

	it('posts one sync for the view and ignores a second pull while busy', async () => {
		const f = await refreshing('label:work');
		void f.list.refresh();
		await flush();
		expect(f.syncCalls).toEqual([
			{ path: `/api/views/${encodeURIComponent('label:work')}/sync`, method: 'POST' }
		]);
	});

	it('ends on a change that moves the list, but not before the minimum spin', async () => {
		const f = await refreshing();
		await at(100);
		f.setDate(60, 600);
		f.send([upsert(60, 600, [INBOX])]);
		expect(f.ids()[0]).toBe(60);
		await at(REFRESH_MIN_SPIN_MS - 101);
		expect(f.list.refreshing).toBe('busy');
		await at(1);
		expect(f.list.refreshing).toBe('idle');
		await at(REFRESH_TIMEOUT_MS);
		expect(f.list.refreshing).toBe('idle');
		expect(f.statusCalls()).toBe(0);
	});

	it('ends at once on a qualifying change after the minimum spin', async () => {
		const f = await refreshing();
		await at(1000);
		f.send([upsert(40, 400, [ARCHIVE])]);
		expect(f.list.refreshing).toBe('idle');
	});

	it('ends on a change that schedules a refetch (a bulk mailbox op)', async () => {
		const f = await refreshing();
		await at(700);
		f.send([{ entity: 'mailbox', id: INBOX, op: { kind: 'bulk', mailbox: INBOX }, payload: null }]);
		expect(f.list.refreshing).toBe('idle');
	});

	it('is not ended by a refetch the window loader asks for', async () => {
		const f = await refreshing();
		await at(700);
		f.list.scheduleRefetch();
		await at(REFETCH_DEBOUNCE_MS);
		await flush();
		expect(f.list.refreshing).toBe('busy');
	});

	it('runs to the neutral timeout on irrelevant changes, then shows up to date', async () => {
		const f = await refreshing();
		await at(100);
		f.send([
			{ entity: 'op', id: 1, op: { kind: 'upsert' }, payload: null },
			{ entity: 'outbox', id: 1, op: { kind: 'upsert' }, payload: null },
			{ entity: 'identity', id: 1, op: { kind: 'upsert' }, payload: null },
			{ entity: 'message', id: 9, op: { kind: 'upsert' }, payload: { threadId: 50 } },
			upsert(70, 700, [ARCHIVE])
		]);
		expect(f.ids()).toEqual([50, 40, 30, 20, 10]);
		await at(REFRESH_TIMEOUT_MS - 101);
		expect(f.list.refreshing).toBe('busy');
		await at(1);
		await flush();
		expect(f.statusCalls()).toBe(1);
		expect(f.list.refreshing).toBe('done');
		await at(UP_TO_DATE_MS - 1);
		expect(f.list.refreshing).toBe('done');
		await at(1);
		expect(f.list.refreshing).toBe('idle');
	});

	it('ends a search view early only on a thread upsert, else at the timeout', async () => {
		const early = await refreshing('search:foo');
		await at(700);
		early.send([upsert(60, 600, [INBOX])]);
		expect(early.list.refreshing).toBe('idle');

		const late = await refreshing('search:foo');
		late.send([{ entity: 'label', id: 1, op: { kind: 'upsert' }, payload: null }]);
		await at(REFRESH_TIMEOUT_MS);
		await flush();
		expect(late.list.refreshing).toBe('done');
	});

	it('reports a failed request after the minimum spin', async () => {
		const f = await ready();
		f.setSync(() =>
			Promise.resolve(Response.json({ error: 'boom', message: 'boom' }, { status: 500 }))
		);
		void f.list.refresh();
		await flush();
		expect(f.list.refreshing).toBe('busy');
		await at(REFRESH_MIN_SPIN_MS - 1);
		expect(f.list.refreshing).toBe('busy');
		await at(1);
		expect(f.list.refreshing).toBe('failed');
		await at(UP_TO_DATE_MS);
		expect(f.list.refreshing).toBe('idle');
		await at(REFRESH_TIMEOUT_MS);
		expect(f.list.refreshing).toBe('idle');
		expect(f.statusCalls()).toBe(0);
	});

	it('waits for a request still pending at the timeout and reports its failure', async () => {
		const f = await ready();
		let fail = (): void => undefined;
		f.setSync(
			() =>
				new Promise<Response>((resolve) => {
					fail = () => resolve(Response.json({ error: 'boom', message: 'boom' }, { status: 500 }));
				})
		);
		void f.list.refresh();
		await at(REFRESH_TIMEOUT_MS);
		await flush();
		expect(f.statusCalls()).toBe(1);
		expect(f.list.refreshing).toBe('busy');
		await at(1000);
		expect(f.list.refreshing).toBe('busy');
		fail();
		await flush();
		expect(f.list.refreshing).toBe('failed');
	});

	it('shows up to date once a request pending at the timeout succeeds', async () => {
		const f = await ready();
		let answer = (): void => undefined;
		f.setSync(
			() =>
				new Promise<Response>((resolve) => {
					answer = () => resolve(new Response(null, { status: 202 }));
				})
		);
		void f.list.refresh();
		await at(REFRESH_TIMEOUT_MS);
		await flush();
		expect(f.list.refreshing).toBe('busy');
		answer();
		await flush();
		expect(f.list.refreshing).toBe('done');
	});

	it('reports offline when the request cannot reach the server', async () => {
		const f = await ready();
		f.setSync(() => Promise.reject(new TypeError('Failed to fetch')));
		void f.list.refresh();
		await at(REFRESH_MIN_SPIN_MS);
		expect(f.list.refreshing).toBe('offline');
	});

	it("shows couldn't refresh when an account of the view has a last error at the timeout", async () => {
		const f = await refreshing();
		f.setLastError(2, 'login failed');
		await at(REFRESH_TIMEOUT_MS);
		await flush();
		expect(f.list.refreshing).toBe('failed');
	});

	it("ignores another account's last error in a scoped view", async () => {
		const f = await refreshing('account:1/inbox');
		f.setLastError(2, 'login failed');
		await at(REFRESH_TIMEOUT_MS);
		await flush();
		expect(f.list.refreshing).toBe('done');
	});

	it('maps a mailbox view to its account for the last error check', async () => {
		const f = await refreshing(`mailbox:${ARCHIVE}`);
		f.setLastError(1, 'login failed');
		await at(REFRESH_TIMEOUT_MS);
		await flush();
		expect(f.list.refreshing).toBe('failed');
	});

	it('can refresh again once the status cleared', async () => {
		const f = await refreshing();
		await at(REFRESH_TIMEOUT_MS);
		await flush();
		expect(f.list.refreshing).toBe('done');
		void f.list.refresh();
		await flush();
		expect(f.list.refreshing).toBe('busy');
		expect(f.syncCalls).toHaveLength(2);
		await at(UP_TO_DATE_MS);
		expect(f.list.refreshing).toBe('busy');
	});
});
