import type { IncomingMessage, ServerResponse } from 'node:http';
import type {
	AttachmentDto,
	DraftDto,
	DraftInputDto,
	IdentityDto,
	LabelChangeDto,
	LabelDto,
	LabelJobDto,
	NewLabelDto,
	NewOpDto,
	OpDto,
	OpKindDto,
	OutboxItemDto,
	ReplyDraftDto,
	SendDto,
	SendStatusDto
} from '../../../packages/api-types/src/index.ts';
import { LABELS, ME, messageById, type Address, type Row } from './fixture.ts';
import { quotedBody } from './render.ts';

/** Mock of the M3 write API, shaped by the generated DTOs. */

export const INBOX = 1;
export const TRASH = 4;
const ARCHIVE = 3;
const SPAM = 5;
const OP_KINDS: ReadonlySet<string> = new Set<OpKindDto>([
	'label.add',
	'label.remove',
	'archive',
	'unarchive',
	'move',
	'markRead',
	'markUnread',
	'star',
	'unstar',
	'trash',
	'spam',
	'delete'
]);
const ATTACHMENT_MAX = 25 * 1024 * 1024;
const OP_DELAY_MS = 150;
const SEND_STEP_MS = 350;
const JOB_STEP_MS = 250;
const JOB_MAILBOXES = 3;

export interface Override {
	labels?: string[];
	starred?: boolean;
	unreadCount?: number;
	mailbox?: number;
	deleted?: boolean;
}

interface MockOp {
	dto: OpDto;
	before: Map<number, Override | undefined>;
}

interface Draft extends DraftDto {
	saves: number;
	/** Saves that asked for the IMAP Drafts mailbox right away (`saveNow`). */
	imapSaves: number;
}

export interface Change {
	entity: string;
	id: number;
	op: { kind: string };
	payload: unknown;
}

export interface WriteContext {
	row: (id: number) => Row | undefined;
	push: (changes: Change[]) => void;
	send: (res: ServerResponse, status: number, body: unknown) => void;
	error: (res: ServerResponse, status: number, code: string, message: string) => void;
	readBody: (req: IncomingMessage) => Promise<Buffer>;
}

export const overrides = new Map<number, Override>();
const ops = new Map<string, MockOp>();
let labels: LabelDto[] = [];
/** Old label key → new key, for rows generated with the fixture's keys. */
const renamed = new Map<string, string>();
const deletedLabels = new Set<string>();
const labelJobs = new Map<number, LabelJobDto>();
const drafts = new Map<number, Draft>();
const attachments = new Map<string, AttachmentDto>();
const outbox: OutboxItemDto[] = [];
const sent: Draft[] = [];
let opsMode: 'ok' | 'reject' | 'fail' = 'ok';
let sendMode: 'ok' | 'fail' = 'ok';
let nextId = 1;
let opSeq = 1;

const IDENTITIES: IdentityDto[] = [
	{ id: 1, accountId: 1, address: ME.email, name: ME.name, signature: null, isDefault: true },
	{
		id: 2,
		accountId: 1,
		address: 'michel@work.example',
		name: ME.name,
		signature: null,
		isDefault: false
	}
];

export function resetWrites(): void {
	overrides.clear();
	ops.clear();
	labels = LABELS.map((l) => ({ ...l, color: null }));
	renamed.clear();
	deletedLabels.clear();
	labelJobs.clear();
	drafts.clear();
	attachments.clear();
	outbox.length = 0;
	sent.length = 0;
	opsMode = 'ok';
	sendMode = 'ok';
}
resetWrites();

export function currentLabels(): LabelDto[] {
	return labels;
}

function mapLabels(keys: readonly string[]): string[] {
	return keys
		.map((k) => renamed.get(k) ?? k)
		.filter((k) => !deletedLabels.has(k))
		.sort();
}

/** The row with every applied op, rename and delete. */
export function applyOverride(r: Row): Row {
	const o = overrides.get(r.id);
	return {
		...r,
		labels: mapLabels(o?.labels ?? r.labels),
		starred: o?.starred ?? r.starred,
		unreadCount: o?.unreadCount ?? r.unreadCount,
		mailboxIds: [mailboxOf(r.id)]
	};
}

export function mailboxOf(id: number): number {
	return overrides.get(id)?.mailbox ?? INBOX;
}

export function isDeleted(id: number): boolean {
	return overrides.get(id)?.deleted === true;
}

/** Whether views have to be filtered at all (the unfiltered inbox index is cached). */
export function anyMoved(): boolean {
	for (const o of overrides.values()) if (o.mailbox !== undefined || o.deleted) return true;
	return false;
}

function effect(op: OpDto, current: Row): Override {
	const o: Override = { ...overrides.get(current.id) };
	switch (op.kind) {
		case 'label.add':
			if (op.label && !current.labels.includes(op.label)) {
				o.labels = [...current.labels, op.label].sort();
			}
			break;
		case 'label.remove':
			o.labels = current.labels.filter((l) => l !== op.label);
			break;
		case 'markRead':
			o.unreadCount = 0;
			break;
		case 'markUnread':
			o.unreadCount = Math.max(1, current.unreadCount);
			break;
		case 'star':
			o.starred = true;
			break;
		case 'unstar':
			o.starred = false;
			break;
		case 'archive':
			if (mailboxOf(current.id) === INBOX) o.mailbox = ARCHIVE;
			break;
		case 'unarchive':
			o.mailbox = INBOX;
			break;
		case 'move':
			o.mailbox = op.mailboxId ?? INBOX;
			break;
		case 'trash':
			o.mailbox = TRASH;
			break;
		case 'spam':
			o.mailbox = SPAM;
			break;
		case 'delete':
			o.deleted = true;
			break;
	}
	return o;
}

function threadChange(ctx: WriteContext, id: number): Change {
	const r = ctx.row(id);
	return {
		entity: 'thread',
		id,
		op: { kind: isDeleted(id) ? 'delete' : 'upsert' },
		payload: r
			? { lastDate: r.lastDate, mailboxes: r.mailboxIds, removedFrom: [], labels: r.labels }
			: null
	};
}

function opChange(op: MockOp): Change {
	return { entity: 'op', id: op.dto.seq, op: { kind: 'upsert' }, payload: { ...op.dto } };
}

function activate(ctx: WriteContext, op: MockOp): void {
	op.before.clear();
	for (const id of op.dto.threadIds) {
		const r = ctx.row(id);
		if (!r) continue;
		op.before.set(id, overrides.get(id));
		overrides.set(id, effect(op.dto, r));
	}
	op.dto.status = 'pending';
	op.dto.error = null;
	op.dto.attempts++;
	ctx.push([...op.dto.threadIds.map((id) => threadChange(ctx, id)), opChange(op)]);
	const failing = opsMode === 'fail';
	setTimeout(() => {
		if (ops.get(op.dto.opId) !== op || op.dto.status !== 'pending') return;
		if (failing) {
			revert(op);
			op.dto.status = 'failed';
			op.dto.error = 'IMAP STORE failed: [CANNOT] Mailbox is read-only';
			ctx.push([...op.dto.threadIds.map((id) => threadChange(ctx, id)), opChange(op)]);
		} else {
			op.dto.status = 'done';
			ctx.push([opChange(op)]);
		}
	}, OP_DELAY_MS);
}

function revert(op: MockOp): void {
	for (const [id, before] of op.before) {
		if (before === undefined) overrides.delete(id);
		else overrides.set(id, before);
	}
	op.before.clear();
}

function labelEvent(ctx: WriteContext, label: LabelDto | { key: string }, kind = 'upsert') {
	ctx.push([{ entity: 'label', id: 0, op: { kind }, payload: label }]);
}

/** A rename or delete job that walks a few mailboxes, reporting each step. */
function startJob(
	ctx: WriteContext,
	job: Omit<LabelJobDto, 'id' | 'accountId' | 'done' | 'total' | 'status' | 'error'>,
	finish: () => void
): number {
	const dto: LabelJobDto = {
		id: nextId++,
		accountId: 1,
		done: 0,
		total: JOB_MAILBOXES,
		status: 'running',
		error: null,
		...job
	};
	labelJobs.set(dto.id, dto);
	const step = () => {
		dto.done++;
		if (dto.done >= dto.total) {
			finish();
			dto.status = 'done';
			labelJobs.delete(dto.id);
		} else setTimeout(step, JOB_STEP_MS);
		ctx.push([{ entity: 'label_job', id: dto.id, op: { kind: 'upsert' }, payload: { ...dto } }]);
	};
	setTimeout(step, JOB_STEP_MS);
	return dto.id;
}

function multipartFile(
	body: Buffer,
	contentType: string
): { name: string; type: string; size: number } {
	const boundary = /boundary=(?:"([^"]+)"|([^;]+))/.exec(contentType);
	const head = body.subarray(0, Math.min(body.length, 2048)).toString('latin1');
	const name = /filename="([^"]*)"/.exec(head)?.[1] ?? 'attachment';
	const type = /Content-Type:\s*([^\r\n]+)/i.exec(head)?.[1] ?? 'application/octet-stream';
	const start = head.indexOf('\r\n\r\n') + 4;
	const marker = boundary ? `\r\n--${boundary[1] ?? boundary[2]}` : '';
	const end = marker ? body.lastIndexOf(marker) : body.length;
	return { name, type, size: Math.max(0, end - start) };
}

function plain(list: Address[]) {
	return list.map((a) => ({ name: a.name, email: a.email }));
}

function notMe(list: Address[]): Address[] {
	return list.filter((a) => a.email.toLowerCase() !== ME.email);
}

function draftOf(id: number, input: DraftInputDto, previous?: Draft): Draft {
	return {
		id,
		accountId: 1,
		identityId: input.identityId,
		to: input.to,
		cc: input.cc,
		bcc: input.bcc,
		subject: input.subject,
		text: input.text ?? null,
		html: input.html ?? null,
		inReplyToMessageId:
			previous?.inReplyToMessageId ??
			(input.inReplyTo ? `${input.inReplyTo}@mock.tagmail.test` : null),
		attachmentIds: input.attachmentIds,
		attachments: input.attachmentIds
			.map((a) => attachments.get(a))
			.filter((a): a is AttachmentDto => !!a),
		version: (previous?.version ?? 0) + 1,
		saved: true,
		saveError: null,
		queued: false,
		saves: (previous?.saves ?? 0) + 1,
		imapSaves: (previous?.imapSaves ?? 0) + (input.saveNow ? 1 : 0)
	};
}

function outboxChange(item: OutboxItemDto, kind = 'upsert'): Change {
	return {
		entity: 'outbox',
		id: item.id,
		op: { kind },
		payload: kind === 'delete' ? null : { status: item.status, error: item.error }
	};
}

function deliver(ctx: WriteContext, item: OutboxItemDto): void {
	const failing = sendMode === 'fail';
	setTimeout(() => {
		if (item.status !== 'queued') return;
		item.status = 'sending';
		item.attempts++;
		ctx.push([outboxChange(item)]);
		setTimeout(() => {
			const draft = item.draftId === null ? undefined : drafts.get(item.draftId);
			if (failing || !draft) {
				item.status = 'failed';
				item.error = 'SMTP 554 5.7.1 Message rejected by the relay';
			} else {
				item.status = 'sent';
				item.sentAt = Math.floor(Date.now() / 1000);
				sent.push(draft);
				drafts.delete(draft.id);
				item.draftId = null;
			}
			ctx.push([outboxChange(item)]);
		}, SEND_STEP_MS);
	}, SEND_STEP_MS);
}

/** Handles a write route; returns false when `path` is not one. */
export async function writeApi(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	const method = req.method ?? 'GET';
	const path = url.pathname;
	const json = async <T>() => {
		const text = (await ctx.readBody(req)).toString('utf8');
		return (text ? JSON.parse(text) : {}) as T;
	};
	const ok = (status: number, body: unknown) => {
		ctx.send(res, status, body);
		return true;
	};
	const fail = (status: number, code: string, message: string) => {
		ctx.error(res, status, code, message);
		return true;
	};

	if (path === '/api/ops' && method === 'POST') {
		const b = await json<Partial<NewOpDto>>();
		if (!b.opId || !b.kind || !OP_KINDS.has(b.kind) || !Array.isArray(b.threadIds)) {
			return fail(422, 'invalid', 'opId, kind and threadIds are required');
		}
		const seen = ops.get(b.opId);
		if (seen) return ok(200, seen.dto);
		if (opsMode === 'reject') return fail(503, 'unavailable', 'the account is offline');
		const op: MockOp = {
			dto: {
				opId: b.opId,
				seq: opSeq++,
				accountId: 1,
				kind: b.kind,
				status: 'pending',
				error: null,
				attempts: 0,
				threadIds: b.threadIds,
				label: b.label ?? null,
				mailboxId: b.mailboxId ?? null,
				createdAt: Math.floor(Date.now() / 1000)
			},
			before: new Map()
		};
		ops.set(op.dto.opId, op);
		activate(ctx, op);
		return ok(202, op.dto);
	}
	if (path === '/api/ops' && method === 'GET') {
		const status = url.searchParams.get('status');
		return ok(200, {
			ops: [...ops.values()]
				.map((o) => o.dto)
				.filter((o) => o.status !== 'done' && (!status || o.status === status))
		});
	}
	let m = /^\/api\/ops\/([^/]+)(\/retry)?$/.exec(path);
	if (m) {
		const op = ops.get(decodeURIComponent(m[1]));
		if (!op) return fail(404, 'not_found', 'no such op');
		if (m[2] && method === 'POST') {
			if (op.dto.status === 'failed') activate(ctx, op);
			return ok(202, op.dto);
		}
		if (!m[2] && method === 'DELETE') {
			if (op.dto.status === 'pending') revert(op);
			ops.delete(op.dto.opId);
			ctx.push([
				...op.dto.threadIds.map((id) => threadChange(ctx, id)),
				{ entity: 'op', id: op.dto.seq, op: { kind: 'delete' }, payload: { opId: op.dto.opId } }
			]);
			return ok(204, '');
		}
		return ok(200, op.dto);
	}

	if (path === '/api/labels' && method === 'POST') {
		const { name, color } = await json<Partial<NewLabelDto>>();
		const trimmed = (name ?? '').trim();
		if (!trimmed) return fail(422, 'invalid', 'a label needs a name');
		const key = trimmed.toLowerCase();
		if (labels.some((l) => l.key === key)) return fail(409, 'label_exists', 'that label exists');
		const label: LabelDto = { key, name: trimmed, color: color ?? null };
		labels.push(label);
		deletedLabels.delete(key);
		labelEvent(ctx, label);
		return ok(201, label);
	}
	m = /^\/api\/labels\/([^/]+)$/.exec(path);
	if (m) {
		const key = decodeURIComponent(m[1]);
		const label = labels.find((l) => l.key === key);
		if (!label) return fail(404, 'not_found', 'no such label');
		if (method === 'PATCH') {
			const { name, color } = await json<{ name?: string; color?: string }>();
			const jobIds: number[] = [];
			if (color !== undefined) label.color = color === '' ? null : color;
			if (name && name.trim() !== label.name) {
				const newKey = name.trim().toLowerCase();
				label.name = name.trim();
				if (newKey !== key) {
					label.key = newKey;
					// Old and new key read as one while the job runs, so views switch at once.
					renamed.set(key, newKey);
					for (const [from, to] of renamed) if (to === key) renamed.set(from, newKey);
					for (const o of overrides.values()) {
						if (o.labels) o.labels = o.labels.map((l) => (l === key ? newKey : l));
					}
					jobIds.push(startJob(ctx, { kind: 'rename', label: key, to: newKey }, () => {}));
					labelEvent(ctx, { key }, 'delete');
				}
			}
			labelEvent(ctx, label);
			const body: LabelChangeDto = { label, jobId: jobIds[0] ?? null, jobIds };
			return ok(200, body);
		}
		if (method === 'DELETE') {
			labels = labels.filter((l) => l !== label);
			const id = startJob(ctx, { kind: 'delete', label: key, to: null }, () =>
				deletedLabels.add(key)
			);
			labelEvent(ctx, { key }, 'delete');
			const body: LabelChangeDto = { label: null, jobId: id, jobIds: [id] };
			return ok(202, body);
		}
	}
	if (path === '/api/label-jobs') return ok(200, { jobs: [...labelJobs.values()] });

	if (path === '/api/identities') return ok(200, { identities: IDENTITIES });
	if (path === '/api/attachments' && method === 'POST') {
		const declared = Number(req.headers['content-length'] ?? 0);
		const body = await ctx.readBody(req);
		const file = multipartFile(body, req.headers['content-type'] ?? '');
		if (declared > ATTACHMENT_MAX + 4096 || file.size > ATTACHMENT_MAX) {
			return fail(413, 'too_large', 'attachments are limited to 25 MB');
		}
		const a: AttachmentDto = {
			id: String(nextId++),
			name: file.name,
			size: file.size,
			contentType: file.type
		};
		attachments.set(a.id, a);
		return ok(201, a);
	}
	m = /^\/api\/attachments\/([^/]+)$/.exec(path);
	if (m && method === 'DELETE') {
		attachments.delete(decodeURIComponent(m[1]));
		return ok(200, {});
	}
	if (path === '/api/drafts' && method === 'POST') {
		const draft = draftOf(nextId++, await json<DraftInputDto>());
		drafts.set(draft.id, draft);
		return ok(201, draft);
	}
	m = /^\/api\/drafts\/(\d+)$/.exec(path);
	if (m) {
		const draft = drafts.get(Number(m[1]));
		if (!draft) return fail(404, 'not_found', 'no such draft');
		if (method === 'PUT') {
			if (draft.queued) return fail(409, 'draft_queued', 'the draft is waiting to be sent');
			const next = draftOf(draft.id, await json<DraftInputDto>(), draft);
			drafts.set(draft.id, next);
			return ok(200, next);
		}
		if (method === 'DELETE') {
			drafts.delete(draft.id);
			return ok(200, {});
		}
		return ok(200, draft);
	}
	if (path === '/api/send' && method === 'POST') {
		const { opId, draftId } = await json<Partial<SendDto>>();
		const earlier = outbox.find((i) => i.opId === opId);
		if (opId && earlier) return ok(200, { opId, status: earlier.status } satisfies SendStatusDto);
		const draft = drafts.get(draftId ?? -1);
		if (!opId || !draft) return fail(422, 'invalid', 'opId and a saved draftId are required');
		draft.queued = true;
		const item: OutboxItemDto = {
			id: nextId++,
			opId,
			accountId: 1,
			draftId: draft.id,
			subject: draft.subject,
			to: [...draft.to, ...draft.cc, ...draft.bcc],
			status: 'queued',
			attempts: 0,
			error: null,
			createdAt: Math.floor(Date.now() / 1000),
			sentAt: null
		};
		outbox.push(item);
		ctx.push([outboxChange(item)]);
		deliver(ctx, item);
		return ok(202, { opId, status: 'queued' } satisfies SendStatusDto);
	}
	if (path === '/api/outbox' && method === 'GET') return ok(200, { items: outbox });
	m = /^\/api\/outbox\/(\d+)(\/retry)?$/.exec(path);
	if (m) {
		const item = outbox.find((i) => i.id === Number(m![1]));
		if (!item) return fail(404, 'not_found', 'no such outbox item');
		if (m[2] && method === 'POST') {
			if (item.status !== 'failed') return fail(409, 'not_failed', 'only failed items retry');
			item.status = 'queued';
			item.error = null;
			ctx.push([outboxChange(item)]);
			deliver(ctx, item);
			return ok(200, item);
		}
		if (!m[2] && method === 'DELETE') {
			if (item.status !== 'queued' && item.status !== 'failed') {
				return fail(409, 'already_sending', 'the message is already on its way');
			}
			outbox.splice(outbox.indexOf(item), 1);
			const draft = item.draftId === null ? undefined : drafts.get(item.draftId);
			if (draft) draft.queued = false;
			ctx.push([outboxChange(item, 'delete')]);
			return ok(200, item);
		}
	}
	m = /^\/api\/messages\/(\d+)\/reply$/.exec(path);
	if (m) {
		const found = messageById(Number(m[1]));
		if (!found) return fail(404, 'not_found', 'no such message');
		const { message: msg } = found;
		const mode = url.searchParams.get('mode') ?? 'reply';
		const base = (msg.subject ?? '').replace(/^(re|fwd?):\s*/i, '');
		const from = msg.from ? [msg.from] : [];
		const who = msg.from?.name ?? msg.from?.email ?? '';
		const quote = `<div><br></div><div>On ${new Date(msg.date * 1000).toUTCString()}, ${who} wrote:</div><blockquote>${quotedBody(found.row, msg)}</blockquote>`;
		const body: ReplyDraftDto = {
			identityId: 1,
			to: mode === 'forward' ? [] : plain(mode === 'all' ? [...from, ...notMe(msg.to)] : from),
			cc: mode === 'all' ? plain(notMe(msg.cc)) : [],
			bcc: [],
			subject: `${mode === 'forward' ? 'Fwd' : 'Re'}: ${base}`,
			html: quote,
			inReplyTo: mode === 'forward' ? null : msg.id,
			forwardOf: mode === 'forward' ? msg.id : null,
			attachments: []
		};
		return ok(200, body);
	}
	return false;
}

/** Test hooks. */
export async function writeMock(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	const body = async () =>
		JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as { mode?: string };
	if (url.pathname === '/__mock/ops-mode' && req.method === 'POST') {
		opsMode = ((await body()).mode ?? 'ok') as typeof opsMode;
		ctx.send(res, 204, '');
		return true;
	}
	if (url.pathname === '/__mock/send-mode' && req.method === 'POST') {
		sendMode = ((await body()).mode ?? 'ok') as typeof sendMode;
		ctx.send(res, 204, '');
		return true;
	}
	if (url.pathname === '/__mock/state') {
		ctx.send(res, 200, {
			ops: [...ops.values()].map((o) => o.dto),
			drafts: [...drafts.values()],
			sent,
			outbox,
			labels
		});
		return true;
	}
	return false;
}
