import type {
	CalConflictDto,
	CalOpInfoDto,
	CalReplyStateDto,
	OpDto,
	OpKindDto
} from '../../../packages/api-types/src/index.ts';
import type { Change, WriteContext } from './write.ts';

/** Calendar ops of the mock server: they share `/api/ops` with the mail ops. */

export type CalOutcome = 'ok' | 'conflict' | 'reply-sent-not-saved' | 'reply-cancelled';

interface CalOp {
	dto: OpDto;
	onDone: (reply: CalReplyStateDto | null) => void;
}

const OP_DELAY_MS = 150;

const ops = new Map<string, CalOp>();
const discarded = new Set<string>();
let outcome: CalOutcome = 'ok';
let seq = 1000;

export function resetCalOps(): void {
	ops.clear();
	discarded.clear();
	outcome = 'ok';
}

export function setCalOutcome(next: CalOutcome): void {
	outcome = next;
}

export const hasCalOp = (opId: string): boolean => ops.has(opId);
export const wasDiscarded = (opId: string): boolean => discarded.has(opId);
export const calOp = (opId: string): OpDto | undefined => ops.get(opId)?.dto;

export function calOpList(status: string | null): OpDto[] {
	return [...ops.values()]
		.map((o) => o.dto)
		.filter((o) => o.status !== 'done' && (!status || o.status === status));
}

function change(op: OpDto, kind = 'upsert'): Change {
	return {
		entity: 'cal_op',
		id: op.seq,
		op: { kind },
		payload: kind === 'delete' ? { opId: op.opId } : { ...op }
	};
}

function settle(ctx: WriteContext, op: CalOp, planned: CalOutcome): void {
	const { dto } = op;
	const info = dto.calendar!;
	if (planned === 'conflict') {
		dto.status = 'failed';
		dto.error = 'the calendar changed on the server (If-Match failed)';
		info.conflict = 'server_won';
		if (info.reply) info.reply = 'sent';
	} else if (planned === 'reply-sent-not-saved') {
		dto.status = 'failed';
		dto.error = 'CalDAV PUT failed: 503 Service Unavailable';
		info.reply = 'sent';
	} else if (planned === 'reply-cancelled') {
		dto.status = 'done';
		dto.error = null;
		info.reply = 'cancelled';
		op.onDone(info.reply);
	} else {
		dto.status = 'done';
		dto.error = null;
		info.conflict = null;
		if (info.reply) info.reply = 'sent';
		op.onDone(info.reply);
	}
	ctx.push([change(dto)]);
}

function run(ctx: WriteContext, op: CalOp, planned: CalOutcome): void {
	op.dto.status = 'pending';
	op.dto.error = null;
	op.dto.attempts++;
	setTimeout(() => {
		if (ops.get(op.dto.opId) === op && op.dto.status === 'pending') settle(ctx, op, planned);
	}, OP_DELAY_MS);
}

export interface NewCalOp {
	opId: string;
	kind: OpKindDto;
	accountId: number;
	uid: string;
	targetAccountId: number;
	calendarHref: string | null;
	reply: CalReplyStateDto | null;
	onDone: (reply: CalReplyStateDto | null) => void;
}

export function startCalOp(ctx: WriteContext, input: NewCalOp): OpDto {
	const info: CalOpInfoDto = {
		targetAccountId: input.targetAccountId,
		calendarHref: input.calendarHref,
		uid: input.uid,
		conflict: null as CalConflictDto | null,
		replyOpId: input.reply ? `${input.opId}#reply` : null,
		reply: input.reply
	};
	const dto: OpDto = {
		opId: input.opId,
		seq: seq++,
		accountId: input.accountId,
		kind: input.kind,
		status: 'pending',
		error: null,
		attempts: 0,
		threadIds: [],
		label: null,
		mailboxId: null,
		createdAt: Math.floor(Date.now() / 1000),
		domain: 'cal',
		calendar: info
	};
	const op: CalOp = { dto, onDone: input.onDone };
	ops.set(dto.opId, op);
	const planned = outcome;
	outcome = 'ok';
	ctx.push([change(dto)]);
	run(ctx, op, planned);
	return dto;
}

export function retryCalOp(ctx: WriteContext, opId: string): OpDto {
	const op = ops.get(opId)!;
	if (op.dto.status === 'failed') run(ctx, op, 'ok');
	return op.dto;
}

export function discardCalOp(ctx: WriteContext, opId: string): void {
	const op = ops.get(opId)!;
	ops.delete(opId);
	discarded.add(opId);
	ctx.push([change(op.dto, 'delete')]);
}
