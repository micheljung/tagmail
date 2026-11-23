import type { ChangesDto, ThreadRowDto } from '@tagmail/api-types';
import type { OpAckDto, OpDto, OpRequest, OpStatus, ViewId } from '../transport';
import {
	describeDone,
	describeIntent,
	inverseOf,
	joinsView,
	leavesView,
	patchRow,
	type Names,
	type OpIntent,
	type ViewContext
} from './effects';

export interface Removed {
	id: number;
	at: number;
}

/** What the ops client needs from a thread list. */
export interface OpList {
	readonly view: ViewId;
	takeOut(ids: readonly number[]): Removed[];
	putBack(entries: readonly Removed[]): void;
	scheduleRefetch(): void;
	patchRow(id: number, patch: (row: ThreadRowDto) => ThreadRowDto): void;
}

export interface OpsApi {
	postOp(op: OpRequest): Promise<OpAckDto>;
	retryOp(opId: string): Promise<OpAckDto>;
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

export type TrackedStatus = 'sending' | OpStatus;

export interface TrackedOp {
	readonly opId: string;
	readonly intent: OpIntent;
	/** The list it was started from, for undo. */
	readonly view: ViewId | null;
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

/** Change payload of an `op` entity. */
interface OpChange {
	opId?: string;
	status?: OpStatus;
	error?: string | null;
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

	describe(op: TrackedOp): string {
		return describeIntent(op.intent, this.#host.names());
	}

	/** Starts an op: applies it here, sends it, and offers Undo. */
	run(intent: OpIntent, options: { view?: ViewId | null; undoOf?: TrackedOp } = {}): TrackedOp {
		const op: TrackedOp = {
			opId: this.#host.uuid?.() ?? crypto.randomUUID(),
			intent,
			view: options.view ?? null,
			status: 'sending',
			error: null,
			serverKnown: false,
			applied: false,
			createdAt: Date.now(),
			removed: []
		};
		this.#ops.set(op.opId, op);
		this.#activate(op, options.undoOf);
		const inverse = op.view ? inverseOf(intent, op.view, this.#host.context()) : null;
		this.#host.notify.show(describeDone(intent, this.#host.names()), {
			key: `op:${op.opId}`,
			actions: inverse ? [{ label: 'Undo', run: () => this.undo(op) }] : []
		});
		void this.#send(op, 0);
		return op;
	}

	undo(op: TrackedOp): TrackedOp | null {
		if (!op.view) return null;
		const inverse = inverseOf(op.intent, op.view, this.#host.context());
		if (!inverse) return null;
		this.#host.notify.dismissKey(`op:${op.opId}`);
		if (op.status === 'failed') {
			void this.discard(op);
			return null;
		}
		return this.run(inverse, { view: op.view, undoOf: op });
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
				intent: {
					kind: dto.kind,
					threadIds: dto.threadIds ?? [],
					...(dto.label ? { label: dto.label } : {}),
					...(dto.mailboxId != null ? { mailboxId: dto.mailboxId } : {})
				},
				view: null,
				status: dto.status,
				error: dto.error ?? null,
				serverKnown: true,
				applied: false,
				createdAt: (dto.createdAt ?? 0) * 1000,
				removed: []
			});
		}
		this.version++;
	}

	/** Change feed: `op` entities carry the executor's verdict. */
	changes(page: ChangesDto): void {
		for (const change of page.changes) {
			if ((change.entity as string) !== 'op') continue;
			const payload = (change.payload ?? {}) as OpChange;
			const op = payload.opId ? this.#ops.get(payload.opId) : undefined;
			if (!op || !payload.status) continue;
			op.serverKnown = true;
			if (payload.status === 'done') this.#done(op);
			else if (payload.status === 'failed' && op.status !== 'failed') {
				this.#fail(op, payload.error ?? 'The mail server refused the change');
			}
		}
	}

	async #send(op: TrackedOp, resends: number): Promise<void> {
		const { kind, threadIds, emailIds, label, mailboxId } = op.intent;
		const request: OpRequest = { opId: op.opId, kind, threadIds };
		if (emailIds) request.emailIds = emailIds;
		if (label !== undefined) request.label = label;
		if (mailboxId !== undefined) request.mailboxId = mailboxId;
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

	#ack(op: TrackedOp, ack: OpAckDto): void {
		if (!this.#ops.has(op.opId)) return;
		op.serverKnown = true;
		if (ack.status === 'done') return this.#done(op);
		if (ack.status === 'failed') return this.#fail(op, 'The mail server refused the change');
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
		for (const list of this.#host.lists()) {
			if (leavesView(op.intent, list.view, ctx)) {
				const entries = list.takeOut(op.intent.threadIds);
				if (entries.length > 0) op.removed.push({ list, entries });
			} else if (joinsView(op.intent, list.view, ctx)) {
				const back = undoOf?.removed.find((r) => r.list === list);
				if (back) list.putBack(back.entries);
				else list.scheduleRefetch();
			}
		}
		op.applied = true;
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
		this.version++;
	}

	#text(e: unknown): string {
		if (this.#host.errorText) return this.#host.errorText(e);
		return e instanceof Error ? e.message : String(e);
	}
}
