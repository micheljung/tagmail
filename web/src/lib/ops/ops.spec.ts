import type { ThreadRowDto } from '@tagmail/api-types';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, type CalOpInfoDto, type NewOpDto, type OpDto, type ViewId } from '../transport';
import type { OpIntent } from './effects';
import {
	MAX_RESENDS,
	OpsStore,
	type OpList,
	type OpPhase,
	type OpsApi,
	type Removed,
	type TrackedOp
} from './ops.svelte';

function row(id: number, patch: Partial<ThreadRowDto> = {}): ThreadRowDto {
	return {
		id,
		accountId: 1,
		subject: `s${id}`,
		snippet: '',
		lastDate: 1000 - id,
		messageCount: 1,
		unreadCount: 1,
		starred: false,
		hasAttachment: false,
		senders: [],
		labels: [],
		mailboxIds: [1],
		...patch
	};
}

/** A list of ids with the membership calls the store makes. */
class FakeList implements OpList {
	ids: number[];
	refetches = 0;
	readonly rows = new Map<number, ThreadRowDto>();
	/** Set by a test: called from `opsChanged`. */
	onOps?: (intent: OpIntent, phase: OpPhase) => void;

	constructor(
		readonly view: ViewId,
		ids: number[]
	) {
		this.ids = [...ids];
		for (const id of ids) this.rows.set(id, row(id));
	}

	takeOut(ids: readonly number[]): Removed[] {
		const out = ids.map((id) => ({ id, at: this.ids.indexOf(id) })).filter((r) => r.at !== -1);
		this.ids = this.ids.filter((id) => !ids.includes(id));
		return out.sort((a, b) => a.at - b.at);
	}

	putBack(entries: readonly Removed[]): void {
		for (const { id, at } of [...entries].sort((a, b) => a.at - b.at)) {
			if (!this.ids.includes(id)) this.ids.splice(at, 0, id);
		}
	}

	scheduleRefetch(): void {
		this.refetches++;
	}

	patchRow(id: number, patch: (r: ThreadRowDto) => ThreadRowDto): void {
		const r = this.rows.get(id);
		if (r) this.rows.set(id, patch(r));
	}

	opsChanged(intent: OpIntent, phase: OpPhase): void {
		this.onOps?.(intent, phase);
	}
}

/** The server's answer for `request` with `status`. */
function dto(request: NewOpDto, status: OpDto['status'], error: string | null = null): OpDto {
	return {
		opId: request.opId,
		seq: 1,
		accountId: 1,
		kind: request.kind,
		status,
		error,
		attempts: 0,
		threadIds: request.threadIds ?? [],
		label: request.label ?? null,
		mailboxId: request.mailboxId ?? null,
		createdAt: 1,
		domain: 'mail',
		calendar: null
	};
}

interface Deferred {
	request: NewOpDto;
	resolve: (ack: OpDto) => void;
	reject: (e: unknown) => void;
}

function setup() {
	const posted: Deferred[] = [];
	const mocks = {
		retryOp: vi.fn((opId: string) =>
			Promise.resolve(dto({ opId, kind: 'trash', threadIds: [4] }, 'pending'))
		),
		discardOp: vi.fn<(opId: string) => Promise<void>>(() => Promise.resolve()),
		ops: vi.fn<(status: 'pending' | 'failed') => Promise<OpDto[]>>(() => Promise.resolve([]))
	};
	const api: OpsApi = {
		postOp: (request) =>
			new Promise((resolve, reject) => posted.push({ request, resolve, reject })),
		retryOp: mocks.retryOp,
		discardOp: mocks.discardOp,
		ops: mocks.ops
	};
	const inbox = new FakeList('inbox', [1, 2, 3, 4]);
	const label = new FakeList('label:work', [2, 4]);
	const toasts: { text: string; key?: string; actions: string[]; run: (() => void)[] }[] = [];
	let n = 0;
	const store = new OpsStore({
		api,
		lists: () => [inbox, label],
		context: () => ({ inboxIds: new Set([1]) }),
		names: () => ({ label: (k) => k.toUpperCase(), mailbox: () => 'Box' }),
		notify: {
			show(text, options = {}) {
				toasts.push({
					text,
					key: options.key,
					actions: (options.actions ?? []).map((a) => a.label),
					run: (options.actions ?? []).map((a) => a.run)
				});
				return toasts.length;
			},
			dismissKey() {}
		},
		uuid: () => `op-${++n}`,
		resendDelayMs: 5
	});
	return { store, mocks, posted, inbox, label, toasts };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

describe('OpsStore', () => {
	beforeEach(() => vi.useRealTimers());
	afterEach(() => vi.useRealTimers());

	it('gives every op a client id and applies it before the server answers', () => {
		const { store, posted, inbox, label, toasts } = setup();
		store.run({ kind: 'archive', threadIds: [2] }, { view: 'inbox' });
		expect(posted[0].request).toEqual({ opId: 'op-1', kind: 'archive', threadIds: [2] });
		expect(inbox.ids).toEqual([1, 3, 4]);
		expect(label.ids).toEqual([2, 4]);
		expect(toasts[0]).toMatchObject({ text: 'Archived 1 conversation', actions: ['Undo'] });
		expect(store.unsynced.map((o) => o.status)).toEqual(['sending']);
	});

	it('applies and sends a quiet op without a toast', () => {
		const { store, posted, toasts } = setup();
		store.run({ kind: 'markRead', threadIds: [2] }, { view: 'inbox', quiet: true });
		expect(posted).toHaveLength(1);
		expect(store.project(row(2)).unreadCount).toBe(0);
		expect(toasts).toEqual([]);
	});

	it('projects pending row changes and keeps them once confirmed', async () => {
		const { store, posted, inbox } = setup();
		const op = store.run({ kind: 'label.add', label: 'work', threadIds: [1] }, { view: 'inbox' });
		expect(store.project(row(1)).labels).toEqual(['work']);
		expect(store.project(row(3)).labels).toEqual([]);
		posted[0].resolve(dto(posted[0].request, 'pending'));
		await flush();
		expect(store.get(op.opId)?.status).toBe('pending');
		store.changes({
			changes: [
				{
					seq: 1,
					entity: 'op',
					id: 0,
					op: { kind: 'upsert' },
					payload: { opId: op.opId, status: 'done' }
				}
			],
			next: 'e:1',
			more: false
		});
		expect(store.get(op.opId)).toBeUndefined();
		expect(inbox.rows.get(1)?.labels).toEqual(['work']);
	});

	it('rolls back and offers Retry when the server refuses', async () => {
		const { store, posted, inbox, toasts } = setup();
		const op = store.run({ kind: 'archive', threadIds: [2, 3] }, { view: 'inbox' });
		expect(inbox.ids).toEqual([1, 4]);
		posted[0].reject(new ApiError(503, { error: 'unavailable', message: 'offline' }));
		await flush();
		expect(inbox.ids).toEqual([1, 2, 3, 4]);
		expect(op.status).toBe('failed');
		expect(op.error).toBe('offline');
		expect(store.failedCount).toBe(1);
		const failure = toasts.at(-1)!;
		expect(failure).toMatchObject({ text: "Couldn't archive 2 conversations", actions: ['Retry'] });
		expect(failure.key).toBe(toasts[0].key);

		failure.run[0]();
		expect(inbox.ids).toEqual([1, 4]);
		expect(posted[1].request.opId).toBe(op.opId);
	});

	it('rolls back when the executor reports a failure', () => {
		const { store, posted, inbox } = setup();
		const op = store.run({ kind: 'star', threadIds: [1] }, { view: 'inbox' });
		posted[0].resolve(dto(posted[0].request, 'pending'));
		store.changes({
			changes: [
				{
					seq: 2,
					entity: 'op',
					id: 0,
					op: { kind: 'upsert' },
					payload: { opId: op.opId, status: 'failed', error: 'read-only' }
				}
			],
			next: 'e:2',
			more: false
		});
		expect(store.project(row(1)).starred).toBe(false);
		expect(op).toMatchObject({ status: 'failed', error: 'read-only', serverKnown: true });
		expect(inbox.ids).toEqual([1, 2, 3, 4]);
	});

	it('re-sends network failures with the same op id and applies them once', async () => {
		const { store, posted, inbox } = setup();
		const op = store.run({ kind: 'archive', threadIds: [1] }, { view: 'inbox' });
		posted[0].reject(new TypeError('Failed to fetch'));
		await new Promise((r) => setTimeout(r, 20));
		expect(posted).toHaveLength(2);
		expect(posted[1].request).toEqual(posted[0].request);
		posted[1].resolve(dto(posted[1].request, 'pending'));
		await flush();
		expect(op.status).toBe('pending');
		expect(inbox.ids).toEqual([2, 3, 4]);
	});

	it('gives up re-sending after a bound', async () => {
		const { store, posted } = setup();
		const op = store.run({ kind: 'markRead', threadIds: [1] }, { view: 'inbox' });
		for (let i = 0; i <= MAX_RESENDS; i++) {
			await vi.waitFor(() => expect(posted).toHaveLength(i + 1));
			posted[i].reject(new TypeError('Failed to fetch'));
		}
		await vi.waitFor(() => expect(op.status).toBe('failed'));
		expect(posted).toHaveLength(MAX_RESENDS + 1);
		expect(new Set(posted.map((p) => p.request.opId))).toEqual(new Set([op.opId]));
	});

	it('retries an executor failure through the retry endpoint', async () => {
		const { store, posted, mocks } = setup();
		const op = store.run({ kind: 'trash', threadIds: [4] }, { view: 'inbox' });
		posted[0].resolve(dto(posted[0].request, 'pending'));
		await flush();
		store.changes({
			changes: [
				{
					seq: 3,
					entity: 'op',
					id: 0,
					op: { kind: 'upsert' },
					payload: { opId: op.opId, status: 'failed' }
				}
			],
			next: 'e:3',
			more: false
		});
		await store.retry(op);
		expect(mocks.retryOp).toHaveBeenCalledWith(op.opId);
		expect(posted).toHaveLength(1);
		expect(op.status).toBe('pending');
	});

	it('undo restores rows to their old positions with the inverse op', () => {
		const { store, posted, inbox } = setup();
		const op = store.run({ kind: 'archive', threadIds: [1, 3] }, { view: 'inbox' });
		expect(inbox.ids).toEqual([2, 4]);
		const [undo] = store.undo(op);
		expect(undo.intent).toEqual({ kind: 'unarchive', threadIds: [1, 3] });
		expect(posted[1].request.kind).toBe('unarchive');
		expect(inbox.ids).toEqual([1, 2, 3, 4]);
	});

	it("undoes a trash from a label list back to each thread's folder", () => {
		const { store, posted, inbox, label } = setup();
		const origins = new Map([
			[2, [1]],
			[4, [3]]
		]);
		const op = store.run({ kind: 'trash', threadIds: [2, 4] }, { view: 'label:work', origins });
		expect(label.ids).toEqual([]);
		expect(inbox.ids).toEqual([1, 3]);
		const undone = store.undo(op);
		expect(undone.map((u) => u.intent)).toEqual([
			{ kind: 'unarchive', threadIds: [2] },
			{ kind: 'move', mailboxId: 3, threadIds: [4] }
		]);
		expect(posted.map((p) => p.request.kind)).toEqual(['trash', 'unarchive', 'move']);
		expect(label.ids).toEqual([2, 4]);
		expect(inbox.ids).toEqual([1, 2, 3, 4]);
	});

	it('follows ops of other tabs and drops discarded ones', () => {
		const { store } = setup();
		const other = dto({ opId: 'tab-2', kind: 'star', threadIds: [9] }, 'pending');
		const page = (kind: 'upsert' | 'delete', payload: unknown) => ({
			changes: [{ seq: 5, entity: 'op' as const, id: 1, op: { kind }, payload }],
			next: 'e:5',
			more: false
		});
		store.changes(page('upsert', other));
		expect(store.unsynced.map((o) => o.opId)).toEqual(['tab-2']);
		expect(store.project(row(9)).starred).toBe(false);
		store.changes(page('delete', { opId: 'tab-2' }));
		expect(store.unsynced).toEqual([]);
	});

	it('ignores change entities it does not know', () => {
		const { store } = setup();
		const change = (entity: string) => ({
			seq: 1,
			entity: entity as 'unknown',
			id: 1,
			op: { kind: 'upsert' as const },
			payload: dto({ opId: 'x', kind: 'star', threadIds: [1] }, 'pending')
		});
		store.changes({
			changes: [change('event'), change('calendar'), change('something_new')],
			next: 'e:1',
			more: false
		});
		expect(store.unsynced).toEqual([]);
	});

	describe('calendar ops', () => {
		const calendar: CalOpInfoDto = {
			targetAccountId: 1,
			calendarHref: '/dav/personal/',
			uid: 'standup-1',
			conflict: null,
			replyOpId: null,
			reply: null
		};
		const rsvp = (status: OpDto['status'], patch: Partial<CalOpInfoDto> = {}): OpDto => ({
			...dto({ opId: 'cal-1', kind: 'cal.rsvp', threadIds: [] }, status),
			domain: 'cal',
			calendar: { ...calendar, ...patch }
		});
		const event = (payload: OpDto | { opId: string }, kind: 'upsert' | 'delete' = 'upsert') => ({
			changes: [{ seq: 2, entity: 'cal_op' as const, id: 1, op: { kind }, payload }],
			next: 'e:2',
			more: false
		});

		it('follows a tracked op through cal_op events and says when it is done', () => {
			const { store } = setup();
			const done = vi.fn();
			store.onCalendarDone(done);
			store.track(rsvp('pending'));
			expect(store.calendarOp('standup-1')).toMatchObject({ status: 'pending' });
			store.changes(event(rsvp('done')));
			expect(store.calendarOp('standup-1')).toBeUndefined();
			expect(done).toHaveBeenCalledOnce();
			const finished = done.mock.calls[0] as [TrackedOp];
			expect(finished[0].calendar?.uid).toBe('standup-1');
		});

		it('keeps a server_won conflict as a failed op with the conflict flag', () => {
			const { store, toasts } = setup();
			store.track(rsvp('pending'));
			store.changes(event({ ...rsvp('failed', { conflict: 'server_won' }), error: 'etag' }));
			expect(store.unsynced).toMatchObject([
				{ status: 'failed', calendar: { conflict: 'server_won' } }
			]);
			expect(store.describe(store.unsynced[0])).toBe('Reply to invitation standup-1');
			expect(toasts).toEqual([]);
		});

		it('re-applies through the retry route and discards through delete', async () => {
			const { store, mocks } = setup();
			store.track(rsvp('failed', { conflict: 'server_won' }));
			mocks.retryOp.mockResolvedValueOnce(rsvp('pending'));
			await store.retry(store.unsynced[0]);
			expect(mocks.retryOp).toHaveBeenCalledWith('cal-1');
			expect(store.unsynced[0]).toMatchObject({ status: 'pending' });
			await store.discard(store.unsynced[0]);
			expect(mocks.discardOp).toHaveBeenCalledWith('cal-1');
			expect(store.unsynced).toEqual([]);
		});

		it('tells listeners about an op that finished before its answer arrived', () => {
			const { store } = setup();
			const done = vi.fn();
			store.onCalendarDone(done);
			store.changes(event(rsvp('done')));
			expect(done).toHaveBeenCalledOnce();
			expect(store.unsynced).toEqual([]);
		});
	});

	it('removing the filtered label drops the thread from that label list only', () => {
		const { store, inbox, label } = setup();
		store.run({ kind: 'label.remove', label: 'work', threadIds: [2] }, { view: 'label:work' });
		expect(label.ids).toEqual([4]);
		expect(inbox.ids).toEqual([1, 2, 3, 4]);
	});

	it('discard forgets the op and asks the server to drop it', async () => {
		const { store, posted, mocks } = setup();
		const op = store.run({ kind: 'archive', threadIds: [1] }, { view: 'inbox' });
		posted[0].resolve(dto(posted[0].request, 'pending'));
		await flush();
		await store.discard(op);
		expect(mocks.discardOp).toHaveBeenCalledWith(op.opId);
		expect(store.unsynced).toEqual([]);
	});

	it('picks up unsynced ops from the server without applying them twice', async () => {
		const { store, mocks, inbox } = setup();
		mocks.ops.mockImplementation((status) =>
			Promise.resolve(
				status === 'failed'
					? [dto({ opId: 'srv-1', kind: 'archive', threadIds: [1] }, 'failed', 'nope')]
					: []
			)
		);
		await store.load();
		expect(store.unsynced).toMatchObject([{ opId: 'srv-1', status: 'failed', serverKnown: true }]);
		expect(inbox.ids).toEqual([1, 2, 3, 4]);
	});

	it('lists the applied read and unread intents, oldest first', async () => {
		const { store, posted } = setup();
		store.run({ kind: 'markRead', threadIds: [1] }, { view: 'inbox' });
		store.run({ kind: 'star', threadIds: [2] }, { view: 'inbox' });
		store.run({ kind: 'markUnread', threadIds: [1, 3] }, { view: 'inbox' });
		expect(store.unreadIntents()).toEqual([
			{ kind: 'markRead', threadIds: [1] },
			{ kind: 'markUnread', threadIds: [1, 3] }
		]);
		posted[0].reject(new ApiError(400, { error: 'bad', message: 'no' }));
		await flush();
		expect(store.unreadIntents()).toEqual([{ kind: 'markUnread', threadIds: [1, 3] }]);
	});

	it('touches threads of applied or sending ops, not failed ones', async () => {
		const { store, posted } = setup();
		store.run({ kind: 'archive', threadIds: [1] }, { view: 'inbox' });
		store.run({ kind: 'star', threadIds: [2] }, { view: 'inbox' });
		expect(store.touches(1)).toBe(true);
		expect(store.touches(3)).toBe(false);
		posted[0].reject(new ApiError(400, { error: 'bad', message: 'no' }));
		posted[1].resolve(dto(posted[1].request, 'pending'));
		await flush();
		expect(store.touches(1)).toBe(false);
		expect(store.touches(2)).toBe(true);
	});

	it('tells lists about each phase once the projection matches it', async () => {
		const { store, posted, inbox } = setup();
		const seen: string[] = [];
		inbox.onOps = (intent, phase) => {
			const live = store.unreadIntents().length;
			const tracked = store.unsynced.length;
			seen.push(`${intent.kind}:${phase}:${live}:${tracked}`);
		};
		const op = store.run({ kind: 'markRead', threadIds: [1] }, { view: 'inbox' });
		posted[0].reject(new ApiError(400, { error: 'bad', message: 'no' }));
		await flush();
		const retried = store.retry(op);
		posted[1].resolve(dto(posted[1].request, 'done'));
		await retried;
		expect(seen).toEqual([
			'markRead:activate:1:1',
			'markRead:deactivate:0:1',
			'markRead:activate:1:1',
			'markRead:done:0:1'
		]);
		expect(store.unsynced).toEqual([]);
	});
});
