import type { IncomingMessage, ServerResponse } from 'node:http';
import { LABELS, ME, messageById, type Address, type Row } from './fixture.ts';
import { quotedBody } from './render.ts';

/** Mock implementation of the M3 write contract (.omc/research/m3-api-contract.md). */

export const INBOX = 1;
export const TRASH = 4;
const OP_KINDS = new Set([
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
const ARCHIVE = 3;
const SPAM = 5;
const ATTACHMENT_MAX = 25 * 1024 * 1024;
const OP_DELAY_MS = 150;
const SEND_STEP_MS = 350;

export interface Override {
	labels?: string[];
	starred?: boolean;
	unreadCount?: number;
	mailbox?: number;
	deleted?: boolean;
}

interface MockOp {
	opId: string;
	kind: string;
	threadIds: number[];
	label: string | null;
	mailboxId: number | null;
	status: 'pending' | 'done' | 'failed';
	error: string | null;
	createdAt: number;
	before: Map<number, Override | undefined>;
}

interface Draft {
	id: number;
	identityId: number;
	to: Address[];
	cc: Address[];
	bcc: Address[];
	subject: string;
	text?: string;
	html?: string;
	inReplyTo?: number | null;
	forwardOf?: number | null;
	attachmentIds: string[];
	saves: number;
}

interface OutboxItem {
	id: number;
	opId: string;
	draftId: number;
	subject: string;
	to: Address[];
	status: 'queued' | 'sending' | 'failed';
	error: string | null;
}

export interface WriteContext {
	row: (id: number) => Row | undefined;
	push: (changes: { entity: string; id: number; op: { kind: string }; payload: unknown }[]) => void;
	send: (res: ServerResponse, status: number, body: unknown) => void;
	error: (res: ServerResponse, status: number, code: string, message: string) => void;
	readBody: (req: IncomingMessage) => Promise<Buffer>;
}

export const overrides = new Map<number, Override>();
const ops = new Map<string, MockOp>();
let labels = LABELS.map((l) => ({ ...l, color: null as string | null }));
const labelJobs: {
	id: number;
	label: string;
	kind: 'rename' | 'delete';
	done: number;
	total: number;
	status: string;
}[] = [];
const drafts = new Map<number, Draft>();
const attachments = new Map<
	string,
	{ id: string; name: string; size: number; contentType: string }
>();
const outbox: OutboxItem[] = [];
const sendOps = new Map<string, OutboxItem | 'sent'>();
const sent: Draft[] = [];
let opsMode: 'ok' | 'reject' | 'fail' = 'ok';
let sendMode: 'ok' | 'fail' = 'ok';
let nextId = 1;

const IDENTITIES = [
	{
		id: 1,
		accountId: 1,
		address: ME.email,
		name: ME.name,
		signature: null,
		isDefault: true
	},
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
	labelJobs.length = 0;
	drafts.clear();
	attachments.clear();
	outbox.length = 0;
	sendOps.clear();
	sent.length = 0;
	opsMode = 'ok';
	sendMode = 'ok';
}

export function currentLabels() {
	return labels;
}

/** The row with every applied op. */
export function applyOverride(r: Row): Row {
	const o = overrides.get(r.id);
	if (!o) return r;
	return {
		...r,
		labels: o.labels ?? r.labels,
		starred: o.starred ?? r.starred,
		unreadCount: o.unreadCount ?? r.unreadCount
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

function effect(op: MockOp, r: Row): Override {
	const o: Override = { ...overrides.get(r.id) };
	const current = applyOverride(r);
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
			if (mailboxOf(r.id) === INBOX) o.mailbox = ARCHIVE;
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

function threadChange(ctx: WriteContext, id: number) {
	const r = ctx.row(id);
	return {
		entity: 'thread',
		id,
		op: { kind: isDeleted(id) ? 'delete' : 'upsert' },
		payload: r ? { lastDate: r.lastDate, mailboxes: [mailboxOf(id)], removedFrom: [] } : null
	};
}

function opChange(op: MockOp) {
	return {
		entity: 'op',
		id: 0,
		op: { kind: 'upsert' },
		payload: { opId: op.opId, status: op.status, error: op.error }
	};
}

function activate(ctx: WriteContext, op: MockOp): void {
	op.before.clear();
	for (const id of op.threadIds) {
		const r = ctx.row(id);
		if (!r) continue;
		op.before.set(id, overrides.get(id));
		overrides.set(id, effect(op, r));
	}
	op.status = 'pending';
	op.error = null;
	ctx.push(op.threadIds.map((id) => threadChange(ctx, id)));
	const failing = opsMode === 'fail';
	setTimeout(() => {
		if (ops.get(op.opId) !== op || op.status !== 'pending') return;
		if (failing) {
			revert(op);
			op.status = 'failed';
			op.error = 'IMAP STORE failed: [CANNOT] Mailbox is read-only';
			ctx.push([...op.threadIds.map((id) => threadChange(ctx, id)), opChange(op)]);
		} else {
			op.status = 'done';
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

function dto(op: MockOp) {
	return {
		opId: op.opId,
		kind: op.kind,
		status: op.status,
		error: op.error,
		threadIds: op.threadIds,
		label: op.label,
		mailboxId: op.mailboxId,
		createdAt: op.createdAt
	};
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

function plain(list: Address[]): Address[] {
	return list.map((a) => ({ name: a.name, email: a.email }));
}

function notMe(list: Address[]): Address[] {
	return list.filter((a) => a.email.toLowerCase() !== ME.email);
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

	if (path === '/api/ops' && method === 'POST') {
		const b = await json<{
			opId?: string;
			kind?: string;
			threadIds?: number[];
			label?: string;
			mailboxId?: number;
		}>();
		if (!b.opId || !b.kind || !OP_KINDS.has(b.kind) || !Array.isArray(b.threadIds)) {
			ctx.error(res, 422, 'invalid', 'opId, kind and threadIds are required');
			return true;
		}
		const seen = ops.get(b.opId);
		if (seen) {
			ctx.send(res, 200, { opId: seen.opId, status: seen.status });
			return true;
		}
		if (opsMode === 'reject') {
			ctx.error(res, 503, 'unavailable', 'the account is offline');
			return true;
		}
		const op: MockOp = {
			opId: b.opId,
			kind: b.kind,
			threadIds: b.threadIds,
			label: b.label ?? null,
			mailboxId: b.mailboxId ?? null,
			status: 'pending',
			error: null,
			createdAt: Math.floor(Date.now() / 1000),
			before: new Map()
		};
		ops.set(op.opId, op);
		activate(ctx, op);
		ctx.send(res, 202, { opId: op.opId, status: 'pending' });
		return true;
	}
	if (path === '/api/ops' && method === 'GET') {
		const status = url.searchParams.get('status');
		ctx.send(res, 200, {
			ops: [...ops.values()]
				.filter((o) => o.status !== 'done' && (!status || o.status === status))
				.map(dto)
		});
		return true;
	}
	let m = /^\/api\/ops\/([^/]+)(\/retry)?$/.exec(path);
	if (m) {
		const op = ops.get(decodeURIComponent(m[1]));
		if (!op) {
			ctx.error(res, 404, 'not_found', 'no such op');
			return true;
		}
		if (m[2] && method === 'POST') {
			if (op.status === 'failed') activate(ctx, op);
			ctx.send(res, 202, { opId: op.opId, status: op.status });
		} else if (!m[2] && method === 'DELETE') {
			if (op.status === 'pending') revert(op);
			ops.delete(op.opId);
			ctx.push(op.threadIds.map((id) => threadChange(ctx, id)));
			ctx.send(res, 204, '');
		} else ctx.send(res, 200, dto(op));
		return true;
	}

	if (path === '/api/labels' && method === 'POST') {
		const { name, color } = await json<{ name?: string; color?: string | null }>();
		const trimmed = (name ?? '').trim();
		if (!trimmed) {
			ctx.error(res, 422, 'invalid', 'a label needs a name');
			return true;
		}
		const key = trimmed.toLowerCase();
		if (labels.some((l) => l.key === key)) {
			ctx.error(res, 409, 'label_exists', 'that label exists');
			return true;
		}
		const label = { key, name: trimmed, color: color ?? null };
		labels.push(label);
		ctx.push([{ entity: 'label', id: 0, op: { kind: 'upsert' }, payload: { key } }]);
		ctx.send(res, 201, label);
		return true;
	}
	m = /^\/api\/labels\/([^/]+)$/.exec(path);
	if (m) {
		const key = decodeURIComponent(m[1]);
		const label = labels.find((l) => l.key === key);
		if (!label) {
			ctx.error(res, 404, 'not_found', 'no such label');
			return true;
		}
		if (method === 'PATCH') {
			const { name, color } = await json<{ name?: string; color?: string | null }>();
			let jobId: number | null = null;
			if (color !== undefined) label.color = color;
			if (name && name.trim() !== label.name) {
				label.name = name.trim();
				jobId = nextId++;
				labelJobs.push({
					id: jobId,
					label: key,
					kind: 'rename',
					done: 0,
					total: 3,
					status: 'running'
				});
			}
			ctx.push([{ entity: 'label', id: 0, op: { kind: 'upsert' }, payload: { key } }]);
			ctx.send(res, 200, { label, jobId });
			return true;
		}
		if (method === 'DELETE') {
			labels = labels.filter((l) => l !== label);
			ctx.push([{ entity: 'label', id: 0, op: { kind: 'delete' }, payload: { key } }]);
			ctx.send(res, 204, '');
			return true;
		}
	}
	if (path === '/api/label-jobs') {
		for (const job of labelJobs) {
			if (job.status !== 'running') continue;
			job.done++;
			if (job.done >= job.total) job.status = 'done';
		}
		ctx.send(res, 200, labelJobs);
		return true;
	}

	if (path === '/api/identities') {
		ctx.send(res, 200, IDENTITIES);
		return true;
	}
	if (path === '/api/attachments' && method === 'POST') {
		const declared = Number(req.headers['content-length'] ?? 0);
		const body = await ctx.readBody(req);
		const file = multipartFile(body, req.headers['content-type'] ?? '');
		if (declared > ATTACHMENT_MAX + 4096 || file.size > ATTACHMENT_MAX) {
			ctx.error(res, 413, 'attachment_too_large', 'attachments are limited to 25 MB');
			return true;
		}
		const a = { id: `att-${nextId++}`, name: file.name, size: file.size, contentType: file.type };
		attachments.set(a.id, a);
		ctx.send(res, 201, a);
		return true;
	}
	m = /^\/api\/attachments\/([^/]+)$/.exec(path);
	if (m && method === 'DELETE') {
		attachments.delete(decodeURIComponent(m[1]));
		ctx.send(res, 204, '');
		return true;
	}
	if (path === '/api/drafts' && method === 'POST') {
		const b = await json<Omit<Draft, 'id' | 'saves'>>();
		const draft: Draft = { ...b, id: nextId++, saves: 1 };
		drafts.set(draft.id, draft);
		ctx.send(res, 201, { id: draft.id });
		return true;
	}
	m = /^\/api\/drafts\/(\d+)$/.exec(path);
	if (m) {
		const draft = drafts.get(Number(m[1]));
		if (!draft) {
			ctx.error(res, 404, 'not_found', 'no such draft');
			return true;
		}
		if (method === 'PUT') {
			const b = await json<Omit<Draft, 'id' | 'saves'>>();
			Object.assign(draft, b, { saves: draft.saves + 1 });
			ctx.send(res, 204, '');
		} else if (method === 'DELETE') {
			drafts.delete(draft.id);
			ctx.send(res, 204, '');
		} else {
			ctx.send(res, 200, {
				...draft,
				attachments: draft.attachmentIds.map((id) => attachments.get(id)).filter(Boolean)
			});
		}
		return true;
	}
	if (path === '/api/send' && method === 'POST') {
		const { opId, draftId } = await json<{ opId?: string; draftId?: number }>();
		const draft = drafts.get(draftId ?? -1);
		if (!opId || !draft) {
			ctx.error(res, 422, 'invalid', 'opId and a saved draftId are required');
			return true;
		}
		if (sendOps.has(opId)) {
			ctx.send(res, 200, { opId, status: 'queued' });
			return true;
		}
		const item: OutboxItem = {
			id: nextId++,
			opId,
			draftId: draft.id,
			subject: draft.subject,
			to: draft.to,
			status: 'queued',
			error: null
		};
		outbox.push(item);
		sendOps.set(opId, item);
		const failing = sendMode === 'fail';
		setTimeout(() => {
			if (!outbox.includes(item)) return;
			item.status = 'sending';
			setTimeout(() => {
				if (failing) {
					item.status = 'failed';
					item.error = 'SMTP 554 5.7.1 Message rejected by the relay';
					return;
				}
				outbox.splice(outbox.indexOf(item), 1);
				sendOps.set(opId, 'sent');
				sent.push(draft);
				drafts.delete(draft.id);
			}, SEND_STEP_MS);
		}, SEND_STEP_MS);
		ctx.send(res, 202, { opId, status: 'queued' });
		return true;
	}
	if (path === '/api/outbox' && method === 'GET') {
		ctx.send(res, 200, outbox);
		return true;
	}
	m = /^\/api\/outbox\/(\d+)$/.exec(path);
	if (m && method === 'DELETE') {
		const item = outbox.find((i) => i.id === Number(m![1]));
		if (!item || item.status === 'sending') {
			ctx.error(res, 409, 'already_sending', 'the message is already on its way');
			return true;
		}
		outbox.splice(outbox.indexOf(item), 1);
		ctx.send(res, 204, '');
		return true;
	}
	m = /^\/api\/messages\/(\d+)\/reply$/.exec(path);
	if (m) {
		const found = messageById(Number(m[1]));
		if (!found) {
			ctx.error(res, 404, 'not_found', 'no such message');
			return true;
		}
		const { message: msg } = found;
		const mode = url.searchParams.get('mode') ?? 'reply';
		const base = (msg.subject ?? '').replace(/^(re|fwd?):\s*/i, '');
		const from = msg.from ? [msg.from] : [];
		const who = msg.from?.name ?? msg.from?.email ?? '';
		const quote = `<div><br></div><div>On ${new Date(msg.date * 1000).toUTCString()}, ${who} wrote:</div><blockquote>${quotedBody(found.row, msg)}</blockquote>`;
		ctx.send(res, 200, {
			identityId: 1,
			to: mode === 'forward' ? [] : plain(mode === 'all' ? [...from, ...notMe(msg.to)] : from),
			cc: mode === 'all' ? plain(notMe(msg.cc)) : [],
			bcc: [],
			subject: `${mode === 'forward' ? 'Fwd' : 'Re'}: ${base}`,
			html: quote,
			inReplyTo: mode === 'forward' ? null : msg.id,
			forwardOf: mode === 'forward' ? msg.id : null,
			attachments: []
		});
		return true;
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
	if (url.pathname === '/__mock/ops-mode' && req.method === 'POST') {
		const b = JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as {
			mode?: typeof opsMode;
		};
		opsMode = b.mode ?? 'ok';
		ctx.send(res, 204, '');
		return true;
	}
	if (url.pathname === '/__mock/send-mode' && req.method === 'POST') {
		const b = JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as {
			mode?: typeof sendMode;
		};
		sendMode = b.mode ?? 'ok';
		ctx.send(res, 204, '');
		return true;
	}
	if (url.pathname === '/__mock/state') {
		ctx.send(res, 200, {
			ops: [...ops.values()].map(dto),
			drafts: [...drafts.values()],
			sent,
			outbox,
			labels
		});
		return true;
	}
	return false;
}
