import type { ChangesDto, ThreadRowDto } from '@tagmail/api-types';
import type { NewOpDto, OpDto, OpStatusDto, ViewId } from '../transport';
import {
	describeDone,
	describeIntent,
	inverseOf,
	joinsView,
	leavesView,
	patchRow,
	type Names,
	type OpIntent,
	type Origins,
	type ViewContext
} from './effects';

export interface Removed {
	id: number;
	at: number;
	/** Unread-first lists: whether the thread counted as unread before the op (ops not applied). */
	wasUnread?: boolean;
}

/** When an op's effect starts showing, stops showing, or is confirmed by the server. */
export type OpPhase = 'activate' | 'deactivate' | 'done';

/** What the ops client needs from a thread list. */
export interface OpList {
	readonly view: ViewId;
	takeOut(ids: readonly number[]): Removed[];
	putBack(entries: readonly Removed[]): void;
	scheduleRefetch(): void;
	patchRow(id: number, patch: (row: ThreadRowDto) => ThreadRowDto): void;
	/** Called after an op is applied or rolled back, and on confirmation before it is forgotten. */
	opsChanged?(intent: OpIntent, phase: OpPhase): void;
}

export interface OpsApi {
	postOp(op: NewOpDto): Promise<OpDto>;
	retryOp(opId: string): Promise<OpDto>;
	discardOp(opId: string): Promise<void>;
	ops(status: 'pending' | 'failed'): Promise<OpDto[]>;
}

export interface Notifier {
	show(
		text: string,
		options?: {
			tone?: 'info' | 'error' | 'progress';
			actions?: { label: string; run: () => void }[];
			key?: string;
			duration?: number;
		}
	): number;
	dismissKey(key: string): void;
}

export interface OpsHost {
	api: OpsApi;
	lists: () => Iterable<OpList>;
	context: () => ViewContext;
	names: () => Names;
	notify: Notifier;
	uuid?: () => string;
	errorText?: (e: unknown) => string;
	/** Delay before re-sending an op whose request never got an answer. */
	resendDelayMs?: number;
}

export type TrackedStatus = 'sending' | OpStatusDto;

export interface TrackedOp {
	readonly opId: string;
	readonly intent: OpIntent;
	/** The list it was started from, for undo. */
	readonly view: ViewId | null;
	/** Where the threads were, for undoing moves. */
	readonly origins?: Origins;
	status: TrackedStatus;
	error: string | null;
	/** The server has a record of it (so retry and discard go through the API). */
	serverKnown: boolean;
	/** Its effects are showing in this tab. */
	applied: boolean;
	createdAt: number;
	removed: { list: OpList; entries: Removed[] }[];
}

/** Network failures are re-sent with the same op id, at most this often. */
export const MAX_RESENDS = 2;

/** Change payload of an `op` entity: the op on upsert, `{opId}` on delete. */
type OpChange = Partial<OpDto> & { opId?: string };

function intentOf(dto: OpDto): OpIntent {
	return {
		kind: dto.kind,
		threadIds: dto.threadIds,
		...(dto.label !== null ? { label: dto.label } : {}),
		...(dto.mailboxId !== null ? { mailboxId: dto.mailboxId } : {})
	};
}

function lowerFirst(s: string): string {
	return s.charAt(0).toLowerCase() + s.slice(1);
}

/**
 * Client side of `POST /api/ops`: every action gets a client op id, shows
 * at once (rows are projected through the active ops, threads leave lists
 * they no longer belong to), and is rolled back with a Retry toast if the
 * server refuses or the executor fails. Re-sends reuse the op id, so the
 * server's op dedupe keeps them single.
 */
export class OpsStore {
	/** Bumped on every change; read it to subscribe. */
	version = $state(0);

	readonly #host: OpsHost;
	readonly #ops = new Map<string, TrackedOp>();
	readonly #settled = new Set<(op: TrackedOp) => void>();

	constructor(host: OpsHost) {
		this.#host = host;
	}

	/** Called when an op is confirmed, so views outside the lists can keep its effect. */
	onSettled(fn: (op: TrackedOp) => void): () => void {
		this.#settled.add(fn);
		return () => this.#settled.delete(fn);
	}

	get(opId: string): TrackedOp | undefined {
		return this.#ops.get(opId);
	}

	/** Snapshots of the ops not yet confirmed: waiting, failed or still sending. */
	get unsynced(): TrackedOp[] {
		void this.version;
		return [...this.#ops.values()].map((op) => ({ ...op }));
	}

	get failedCount(): number {
		void this.version;
		let n = 0;
		for (const op of this.#ops.values()) if (op.status === 'failed') n++;
		return n;
	}

	/** `row` with every active op applied. */
	project(row: ThreadRowDto): ThreadRowDto;
	project(row: ThreadRowDto | undefined): ThreadRowDto | undefined;
	project(row: ThreadRowDto | undefined): ThreadRowDto | undefined {
		void this.version;
		if (!row || this.#ops.size === 0) return row;
		let out = row;
		for (const op of this.#ops.values()) {
			if (op.applied && op.intent.threadIds.includes(row.id)) out = patchRow(out, op.intent);
		}
		return out;
	}

	/** The applied markRead/markUnread intents, oldest first. */
	unreadIntents(): OpIntent[] {
		void this.version;
		const out: OpIntent[] = [];
		for (const op of this.#ops.values()) {
			if (op.applied && (op.intent.kind === 'markRead' || op.intent.kind === 'markUnread')) {
				out.push(op.intent);
			}
		}
		return out;
	}

	/** Whether an op that is showing or still being sent involves thread `id`. */
	touches(id: number): boolean {
		for (const op of this.#ops.values()) {
			if ((op.applied || op.status === 'sending') && op.intent.threadIds.includes(id)) return true;
		}
		return false;
	}

	describe(op: TrackedOp): string {
		return describeIntent(op.intent, this.#host.names());
	}

	/** Starts an op: applies it here, sends it, and offers Undo. */
	run(
		intent: OpIntent,
		options: { view?: ViewId | null; origins?: Origins; undoOf?: TrackedOp; quiet?: boolean } = {}
	): TrackedOp {
		const op: TrackedOp = {
			opId: this.#host.uuid?.() ?? crypto.randomUUID(),
			intent,
			view: options.view ?? null,
			origins: options.origins,
			status: 'sending',
			error: null,
			serverKnown: false,
			applied: false,
			createdAt: Date.now(),
			removed: []
		};
		this.#ops.set(op.opId, op);
		this.#activate(op, options.undoOf);
		const inverse = this.#inverse(op);
		if (!options.quiet) {
			this.#host.notify.show(describeDone(intent, this.#host.names()), {
				key: `op:${op.opId}`,
				actions: inverse ? [{ label: 'Undo', run: () => this.undo(op) }] : []
			});
		}
		void this.#send(op, 0);
		return op;
	}

	/** Takes `op` back with its inverse ops; returns them. */
	undo(op: TrackedOp): TrackedOp[] {
		const inverse = this.#inverse(op);
		if (!inverse) return [];
		this.#host.notify.dismissKey(`op:${op.opId}`);
		if (op.status === 'failed') {
			void this.discard(op);
			return [];
		}
		return inverse.map((intent) => this.run(intent, { view: op.view, undoOf: op }));
	}

	#inverse(op: TrackedOp): OpIntent[] | null {
		if (!op.view && !op.origins) return null;
		return inverseOf(op.intent, op.view ?? 'inbox', this.#host.context(), op.origins);
	}

	async retry(snapshot: TrackedOp): Promise<void> {
		const op = this.#ops.get(snapshot.opId);
		if (!op || op.status !== 'failed') return;
		op.status = 'sending';
		op.error = null;
		this.#activate(op);
		this.#host.notify.dismissKey(`op:${op.opId}`);
		this.version++;
		if (!op.serverKnown) return this.#send(op, 0);
		try {
			this.#ack(op, await this.#host.api.retryOp(op.opId));
		} catch (e) {
			this.#fail(op, e);
		}
	}

	async discard(snapshot: TrackedOp): Promise<void> {
		const op = this.#ops.get(snapshot.opId) ?? snapshot;
		this.#deactivate(op);
		this.#ops.delete(op.opId);
		this.#host.notify.dismissKey(`op:${op.opId}`);
		this.version++;
		if (op.serverKnown) {
			try {
				await this.#host.api.discardOp(op.opId);
			} catch (e) {
				this.#host.notify.show(`Couldn't discard: ${this.#text(e)}`, { tone: 'error' });
			}
		}
	}

	/** Picks up ops other tabs or an earlier visit left unsynced. */
	async load(): Promise<void> {
		const [pending, failed] = await Promise.all([
			this.#host.api.ops('pending'),
			this.#host.api.ops('failed')
		]);
		for (const dto of [...pending, ...failed]) {
			const known = this.#ops.get(dto.opId);
			if (known) {
				known.serverKnown = true;
				continue;
			}
			this.#ops.set(dto.opId, {
				opId: dto.opId,
				intent: intentOf(dto),
				view: null,
				status: dto.status,
				error: dto.error ?? null,
				serverKnown: true,
				applied: false,
				createdAt: dto.createdAt * 1000,
				removed: []
			});
		}
		this.version++;
	}

	/** Change feed: `op` entities carry the executor's verdict. */
	changes(page: ChangesDto): void {
		for (const change of page.changes) {
			if (change.entity !== 'op') continue;
			const payload = (change.payload ?? {}) as OpChange;
			const op = payload.opId ? this.#ops.get(payload.opId) : undefined;
			if (change.op.kind === 'delete') {
				if (op) this.#forget(op);
				continue;
			}
			if (!payload.opId || !payload.status) continue;
			if (!op) {
				if (payload.status !== 'done' && payload.kind) this.#adopt(payload as OpDto);
				continue;
			}
			op.serverKnown = true;
			if (payload.status === 'done') this.#done(op);
			else if (payload.status === 'failed' && op.status !== 'failed') {
				this.#fail(op, payload.error ?? 'The mail server refused the change');
			}
		}
	}

	/** An op another tab or device started: listed, its effect is in the server's rows. */
	#adopt(dto: OpDto): void {
		this.#ops.set(dto.opId, {
			opId: dto.opId,
			intent: intentOf(dto),
			view: null,
			status: dto.status,
			error: dto.error,
			serverKnown: true,
			applied: false,
			createdAt: dto.createdAt * 1000,
			removed: []
		});
		this.version++;
	}

	#forget(op: TrackedOp): void {
		this.#deactivate(op);
		this.#ops.delete(op.opId);
		this.#host.notify.dismissKey(`op:${op.opId}`);
		this.version++;
	}

	async #send(op: TrackedOp, resends: number): Promise<void> {
		const { kind, threadIds, emailIds, label, mailboxId } = op.intent;
		const request: NewOpDto = { opId: op.opId, kind, threadIds };
		if (emailIds) request.emailIds = emailIds;
		if (label != null) request.label = label;
		if (mailboxId != null) request.mailboxId = mailboxId;
		try {
			this.#ack(op, await this.#host.api.postOp(request));
		} catch (e) {
			if (!this.#ops.has(op.opId)) return;
			if (e instanceof TypeError && resends < MAX_RESENDS) {
				await new Promise((r) => setTimeout(r, this.#host.resendDelayMs ?? 800));
				if (this.#ops.get(op.opId)?.status === 'sending') return this.#send(op, resends + 1);
				return;
			}
			this.#fail(op, e);
		}
	}

	#ack(op: TrackedOp, ack: OpDto): void {
		if (!this.#ops.has(op.opId)) return;
		op.serverKnown = true;
		if (ack.status === 'done') return this.#done(op);
		if (ack.status === 'failed') {
			return this.#fail(op, ack.error ?? 'The mail server refused the change');
		}
		if (op.status === 'sending') op.status = 'pending';
		this.version++;
	}

	#done(op: TrackedOp): void {
		if (op.applied) {
			for (const list of this.#host.lists()) {
				for (const id of op.intent.threadIds) list.patchRow(id, (r) => patchRow(r, op.intent));
			}
		}
		const wasApplied = op.applied;
		op.applied = false;
		op.status = 'done';
		this.#notify(op, 'done');
		this.#ops.delete(op.opId);
		this.version++;
		if (wasApplied) for (const fn of this.#settled) fn(op);
	}

	#fail(op: TrackedOp, e: unknown): void {
		if (!this.#ops.has(op.opId)) return;
		this.#deactivate(op);
		op.status = 'failed';
		op.error = typeof e === 'string' ? e : this.#text(e);
		this.version++;
		this.#host.notify.show(`Couldn't ${lowerFirst(this.describe(op))}`, {
			tone: 'error',
			key: `op:${op.opId}`,
			actions: [{ label: 'Retry', run: () => void this.retry(op) }]
		});
	}

	#activate(op: TrackedOp, undoOf?: TrackedOp): void {
		if (op.applied) return;
		const ctx = this.#host.context();
		const mine = new Set(op.intent.threadIds);
		for (const list of this.#host.lists()) {
			const back = undoOf?.removed
				.find((r) => r.list === list)
				?.entries.filter((e) => mine.has(e.id));
			if (back?.length) {
				list.putBack(back);
			} else if (leavesView(op.intent, list.view, ctx)) {
				const entries = list.takeOut(op.intent.threadIds);
				if (entries.length > 0) op.removed.push({ list, entries });
			} else if (joinsView(op.intent, list.view, ctx)) {
				list.scheduleRefetch();
			}
		}
		op.applied = true;
		this.#notify(op, 'activate');
		this.version++;
	}

	#deactivate(op: TrackedOp): void {
		if (!op.applied) return;
		for (const { list, entries } of op.removed) list.putBack(entries);
		const ctx = this.#host.context();
		for (const list of this.#host.lists()) {
			if (joinsView(op.intent, list.view, ctx)) list.scheduleRefetch();
		}
		op.removed = [];
		op.applied = false;
		this.#notify(op, 'deactivate');
		this.version++;
	}

	#notify(op: TrackedOp, phase: OpPhase): void {
		for (const list of this.#host.lists()) list.opsChanged?.(op.intent, phase);
	}

	#text(e: unknown): string {
		if (this.#host.errorText) return this.#host.errorText(e);
		return e instanceof Error ? e.message : String(e);
	}
}
