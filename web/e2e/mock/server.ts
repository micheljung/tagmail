import { createReadStream, existsSync, statSync } from 'node:fs';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';
import { extname, join, normalize, resolve } from 'node:path';
import { accountsApi, accountsMock, isMulti, mailboxes, setScenario } from './accounts.ts';
import {
	B_THREAD_COUNT,
	PASSWORD,
	SETUP_TOKEN,
	THREAD_COUNT,
	bIdAt,
	bPositionOf,
	bRowAt,
	idAt,
	isSent,
	messageById,
	messagesOf,
	partsOf,
	positionOf,
	remoteImages,
	rowAt,
	type Row
} from './fixture.ts';
import { desktopApi, desktopHeaders, desktopMock, resetDesktop } from './desktop.ts';
import { calendarApi, calendarMock, hasInvitation, resetCaldav } from './calendar.ts';
import { pushApi, pushMock, resetPush } from './push.ts';
import { encodeIdIndex } from './idIndex.ts';
import { RENDER_CSP, WEB_APP_CSP, renderMessage } from './render.ts';
import {
	INBOX,
	accountOf,
	anyMoved,
	applyOverride,
	currentLabels,
	isDeleted,
	isInboxBox,
	isTrashBox,
	mailboxOf,
	resetWrites,
	writeApi,
	writeMock,
	type WriteContext
} from './write.ts';

const PORT = Number(process.env.PORT ?? 4180);
const ROOT = resolve(process.env.WEB_ROOT ?? join(import.meta.dirname, '../../build'));
const LATENCY_MS = Number(process.env.MOCK_LATENCY_MS ?? 8);
const EPOCH = '000000000000000a';
const COOKIE = 'tagmail_session';
const SESSION_TOKEN = 'mock-session-token';
const MAX_IDS = 100;
/** A thread id that was merged into the newest thread. */
const MERGED_ID = 999_999;

const TYPES: Record<string, string> = {
	'.html': 'text/html; charset=utf-8',
	'.js': 'text/javascript',
	'.css': 'text/css',
	'.svg': 'image/svg+xml',
	'.woff2': 'font/woff2',
	'.json': 'application/json',
	'.webmanifest': 'application/manifest+json',
	'.png': 'image/png',
	'.txt': 'text/plain'
};

let seq = 100;
const extraRows = new Map<number, Row>();
const extraOrder: number[] = [];
const sseClients = new Set<ServerResponse>();
const DAY = 86_400;
const T0 = 1_795_000_000;
let ownerExists = true;
const imageSenders = new Set<string>();
const initialSessions = () => [
	{
		id: 'c2Vzc2lvbi0x',
		current: true,
		userAgent: 'Mozilla/5.0 (X11; Linux x86_64) Chrome/147.0 Safari/537.36',
		createdAt: T0,
		lastSeenAt: T0 + 4 * DAY,
		expiresAt: T0 + 30 * DAY,
		idleExpiresAt: T0 + 18 * DAY
	},
	{
		id: 'c2Vzc2lvbi0y',
		current: false,
		userAgent:
			'Mozilla/5.0 (iPhone; CPU iPhone OS 19_1 like Mac OS X) Version/19.1 Mobile Safari/604.1',
		createdAt: T0 - 10 * DAY,
		lastSeenAt: T0 + 2 * DAY,
		expiresAt: T0 + 20 * DAY,
		idleExpiresAt: T0 + 16 * DAY
	}
];
const initialDevices = () => [
	{ id: 1, name: 'Workstation', createdAt: T0 - 20 * DAY, lastSeenAt: T0 + 3 * DAY }
];
let sessions = initialSessions();
let devices = initialDevices();
const currentAuth = () => ({ via: 'session', session: sessions[0], deviceId: null });

const baseIds = Array.from({ length: THREAD_COUNT }, (_, i) => idAt(i));
const inboxIndex = encodeIdIndex(baseIds);
let mergedIds: number[] | null = null;

/** Both accounts' threads, newest first. */
function multiIds(): number[] {
	if (mergedIds) return mergedIds;
	const out: number[] = [];
	let j = 0;
	for (let i = 0; i < THREAD_COUNT; i++) {
		const date = rowAt(i).lastDate;
		while (j < B_THREAD_COUNT && bRowAt(j).lastDate >= date) out.push(bIdAt(j++));
		out.push(baseIds[i]);
	}
	while (j < B_THREAD_COUNT) out.push(bIdAt(j++));
	mergedIds = out;
	return out;
}

function cursor(): string {
	return `${EPOCH}:${seq}`;
}

function row(id: number): Row | undefined {
	if (isDeleted(id)) return undefined;
	const extra = extraRows.get(id);
	if (extra) return applyOverride(extra);
	const at = positionOf(id);
	if (at !== undefined) return applyOverride(rowAt(at));
	const bAt = isMulti() ? bPositionOf(id) : undefined;
	return bAt === undefined ? undefined : applyOverride(bRowAt(bAt));
}

function allIds(): number[] {
	return [...extraOrder, ...(isMulti() ? multiIds() : baseIds)];
}

class BadView extends Error {}

/** Whether `r` passes one search term; `undefined` for terms the mock ignores. */
function matchesTerm(r: Row, i: number, term: string): boolean | undefined {
	if (term === 'is:starred') return r.starred;
	if (term === 'is:unread') return r.unreadCount > 0;
	if (term === 'in:sent') return isSent(i);
	if (term === 'in:inbox') return isInboxBox(mailboxOf(r.id));
	if (term === 'has:attachment') return r.hasAttachment;
	if (term.startsWith('label:')) return r.labels.includes(term.slice(6));
	if (term.startsWith('from:')) {
		const who = term.slice(5);
		return r.senders.some((s) => s.email.includes(who) || s.name?.toLowerCase().includes(who));
	}
	if (!term.includes(':')) {
		return `${r.subject} ${r.snippet}`.toLowerCase().includes(term.replace(/"/g, ''));
	}
	return undefined;
}

function matches(r: Row, i: number, query: string): boolean {
	for (const term of query.toLowerCase().split(/\s+/).filter(Boolean)) {
		const negated = term.length > 1 && term.startsWith('-');
		const hit = matchesTerm(r, i, negated ? term.slice(1) : term);
		if (hit !== undefined && hit === negated) return false;
	}
	return true;
}

function viewIds(view: string): number[] | null {
	const scoped = /^account:(\d+)\/(.+)$/.exec(view);
	if (scoped) {
		if (scoped[2].startsWith('mailbox:')) throw new BadView();
		const account = Number(scoped[1]);
		return (viewIds(scoped[2]) ?? allIds()).filter((id) => accountOf(id) === account);
	}
	const moved = anyMoved();
	if (view === 'inbox' || (view === `mailbox:${INBOX}` && !isMulti())) {
		return moved ? allIds().filter((id) => isInboxBox(mailboxOf(id)) && !isDeleted(id)) : null;
	}
	if (view.startsWith('mailbox:')) {
		const box = Number(view.slice('mailbox:'.length));
		return allIds().filter((id) => mailboxOf(id) === box && !isDeleted(id));
	}
	let query: string;
	if (view.startsWith('label:')) query = `label:${view.slice(6)}`;
	else if (view.startsWith('search:')) query = view.slice(7);
	else return [];
	const visible = (id: number) => !moved || (!isDeleted(id) && !isTrashBox(mailboxOf(id)));
	const out: number[] = [];
	for (const id of allIds()) {
		if (!visible(id)) continue;
		const r = row(id);
		if (r && matches(r, positionOf(id) ?? -1, query)) out.push(id);
	}
	return out;
}

function send(
	res: ServerResponse,
	status: number,
	body: unknown,
	headers: Record<string, string> = {}
) {
	const data = typeof body === 'string' || body instanceof Uint8Array ? body : JSON.stringify(body);
	res.writeHead(status, {
		'content-type':
			typeof body === 'string'
				? 'text/plain'
				: body instanceof Uint8Array
					? 'application/octet-stream'
					: 'application/json',
		'cache-control': 'no-store',
		'x-content-type-options': 'nosniff',
		...headers
	});
	res.end(data);
}

function error(res: ServerResponse, status: number, code: string, message: string) {
	send(res, status, { error: code, message });
}

function authed(req: IncomingMessage): boolean {
	return (req.headers.cookie ?? '').split(/;\s*/).includes(`${COOKIE}=${SESSION_TOKEN}`);
}

async function readBody(req: IncomingMessage): Promise<Buffer> {
	const chunks: Buffer[] = [];
	for await (const c of req) chunks.push(c as Buffer);
	return Buffer.concat(chunks);
}

async function body(req: IncomingMessage): Promise<Record<string, string>> {
	const text = (await readBody(req)).toString('utf8');
	return text ? (JSON.parse(text) as Record<string, string>) : {};
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function login(res: ServerResponse, status = 200) {
	send(res, status, currentAuth(), {
		'set-cookie': `${COOKIE}=${SESSION_TOKEN}; HttpOnly; SameSite=Strict; Path=/`
	});
}

/** What follows `POST /api/views/{view}/sync`; set with `POST /__mock/sync-mode`. */
type SyncMode = 'change' | 'irrelevant' | 'none' | 'fail' | 'engine-error';
const SYNC_CHANGE_DELAY_MS = 100;
/** A thread id outside every view, for the `irrelevant` mode's upsert. */
const OUTSIDE_THREAD_ID = 888_888;
let syncMode: SyncMode = 'none';
/** The server's `--render-author-css`; set with `POST /__mock/render-author-css`. */
let authorCss = false;
/** Decoded view ids of the sync requests, in order. */
const syncCalls: string[] = [];

/** A new INBOX thread delivered through the change feed. */
function deliverNewMail(subject?: string): number {
	const id = THREAD_COUNT + 1000 + extraOrder.length;
	const r: Row = {
		...rowAt(0),
		id,
		subject: subject ?? 'Fresh off the wire',
		snippet: 'Delivered through the change feed while you were looking',
		lastDate: Math.floor(Date.now() / 1000),
		unreadCount: 1,
		messageCount: 1,
		labels: []
	};
	extraRows.set(id, r);
	extraOrder.unshift(id);
	pushChanges([
		{
			entity: 'thread',
			id,
			op: { kind: 'upsert' },
			payload: { accountId: r.accountId, lastDate: r.lastDate, mailboxes: [1], removedFrom: [] }
		}
	]);
	return id;
}

/**
 * Changes no list acts on: op, outbox and identity deletes of unknown ids,
 * and an upsert of an unknown thread placed in Archive with no `lastDate`.
 */
function pushIrrelevantChanges() {
	pushChanges([
		{ entity: 'op', id: 0, op: { kind: 'delete' }, payload: { opId: 'mock-sync-op' } },
		{ entity: 'outbox', id: OUTSIDE_THREAD_ID, op: { kind: 'delete' }, payload: null },
		{ entity: 'identity', id: OUTSIDE_THREAD_ID, op: { kind: 'delete' }, payload: null },
		{
			entity: 'thread',
			id: OUTSIDE_THREAD_ID,
			op: { kind: 'upsert' },
			payload: { accountId: 1, mailboxes: [3], removedFrom: [], labels: [] }
		}
	]);
}

/** `/api/status` with `lastError` set on every account. */
async function statusWithEngineError(req: IncomingMessage, res: ServerResponse, url: URL) {
	await accountsApi(req, res, url, {
		...writeContext,
		send: (r, status, b) => {
			const { accounts } = b as { accounts: object[] };
			const failing = accounts.map((a) => ({ ...a, lastError: 'imap: connection refused' }));
			send(r, status, { accounts: failing });
		}
	});
}

function pushChanges(changes: unknown[]) {
	if (changes.length === 0) return;
	seq++;
	changes = changes.map((c) => ({ ...(c as object), seq }));
	const data = JSON.stringify({ changes, next: cursor(), more: false });
	for (const client of sseClients)
		client.write(`event: changes\nid: ${cursor()}\ndata: ${data}\n\n`);
}

async function api(req: IncomingMessage, res: ServerResponse, url: URL) {
	const method = req.method ?? 'GET';
	const path = url.pathname;
	if (method !== 'GET' && method !== 'HEAD' && req.headers['x-requested-with'] !== 'tagmail') {
		return error(res, 403, 'csrf', 'missing X-Requested-With');
	}
	if (await desktopApi(req, res, url, writeContext)) return;
	if (path === '/api/auth/login' && method === 'POST') {
		const { password } = await body(req);
		if (!ownerExists) return error(res, 401, 'setup_required', 'set up the owner first');
		return password === PASSWORD
			? login(res)
			: error(res, 401, 'invalid_credentials', 'wrong password');
	}
	if (path === '/api/auth/setup' && method === 'POST') {
		const { token, password } = await body(req);
		if (ownerExists) return error(res, 409, 'already_set_up', 'the owner exists');
		if (token !== SETUP_TOKEN) return error(res, 403, 'invalid_setup_token', 'bad token');
		if ((password ?? '').length < 8) {
			return send(res, 422, {
				error: 'invalid',
				message: 'at least 8 characters',
				field: 'password'
			});
		}
		ownerExists = true;
		return login(res, 201);
	}
	if (path === '/api/auth/passkey/login/start' && method === 'POST') {
		return error(res, 404, 'no_passkeys', 'no passkeys registered');
	}
	if (!ownerExists) return error(res, 401, 'setup_required', 'set up the owner first');
	if (!authed(req)) return error(res, 401, 'unauthorized', 'sign in first');
	if (path === '/api/auth/session') return send(res, 200, currentAuth());
	if (path === '/api/auth/logout' && method === 'POST') {
		res.setHeader('set-cookie', `${COOKIE}=; Max-Age=0; Path=/`);
		return send(res, 204, '');
	}
	if (path === '/api/status' && syncMode === 'engine-error') {
		return statusWithEngineError(req, res, url);
	}
	if (await accountsApi(req, res, url, writeContext)) return;
	if (await pushApi(req, res, url, writeContext)) return;
	if (await calendarApi(req, res, url, writeContext)) return;
	if (await writeApi(req, res, url, writeContext)) return;
	if (path === '/api/mailboxes') return send(res, 200, mailboxes());
	if (path === '/api/labels') return send(res, 200, currentLabels());
	if (path === '/api/sessions') return send(res, 200, { sessions });
	if (path === '/api/devices') return send(res, 200, { devices });
	if (path === '/api/passkeys') return send(res, 200, { passkeys: [] });
	if (path === '/api/pairing' && method === 'POST') {
		return send(res, 201, {
			code: 'K7QD2XPM',
			expiresAt: Math.floor(Date.now() / 1000) + 300,
			url: `tagmail://pair?server=${encodeURIComponent(url.origin)}&code=K7QD2XPM`
		});
	}
	let m = /^\/api\/sessions\/([^/]+)$/.exec(path);
	if (m && method === 'DELETE') {
		sessions = sessions.filter((s) => s.id !== decodeURIComponent(m![1]));
		return send(res, 204, '');
	}
	m = /^\/api\/devices\/(\d+)$/.exec(path);
	if (m && method === 'DELETE') {
		devices = devices.filter((d) => d.id !== Number(m![1]));
		return send(res, 204, '');
	}
	m = /^\/api\/views\/([^/]+)\/sync$/.exec(path);
	if (m && method === 'POST') {
		syncCalls.push(decodeURIComponent(m[1]));
		await sleep(LATENCY_MS);
		if (syncMode === 'fail') return error(res, 500, 'internal', 'mock sync failure');
		if (syncMode === 'change') setTimeout(() => deliverNewMail(), SYNC_CHANGE_DELAY_MS);
		if (syncMode === 'irrelevant') setTimeout(pushIrrelevantChanges, SYNC_CHANGE_DELAY_MS);
		res.writeHead(202, { 'cache-control': 'no-store', 'x-content-type-options': 'nosniff' });
		return res.end();
	}
	m = /^\/api\/views\/([^/]+)\/index$/.exec(path);
	if (m) {
		await sleep(LATENCY_MS);
		const view = decodeURIComponent(m[1]);
		let ids: number[] | null;
		try {
			ids = viewIds(view);
		} catch (e) {
			if (e instanceof BadView)
				return error(res, 400, 'bad_request', 'mailbox views are per account');
			throw e;
		}
		const bytes =
			ids === null
				? extraOrder.length || isMulti()
					? encodeIdIndex(allIds())
					: inboxIndex
				: encodeIdIndex(ids);
		return send(res, 200, bytes, { 'x-tagmail-cursor': cursor(), ...desktopHeaders(view) });
	}
	if (path === '/api/threads') {
		const ids = (url.searchParams.get('ids') ?? '').split(',').map(Number);
		if (ids.length > MAX_IDS || ids.some((id) => !Number.isSafeInteger(id))) {
			return error(res, 400, 'bad_request', 'ids must be comma-separated thread ids');
		}
		await sleep(LATENCY_MS);
		const threads = ids.map(row).filter((r): r is Row => r !== undefined);
		return send(res, 200, { cursor: cursor(), threads });
	}
	m = /^\/api\/threads\/(\d+)$/.exec(path);
	if (m) {
		const requested = Number(m[1]);
		const r = row(requested === MERGED_ID ? idAt(0) : requested);
		if (!r) return error(res, 404, 'not_found', 'no such thread');
		await sleep(LATENCY_MS);
		return send(res, 200, { thread: r, messages: messagesOf(r) });
	}
	if (path === '/api/image-senders' && method === 'POST') {
		const { address, allow } = (await body(req)) as unknown as { address: string; allow: boolean };
		if (allow) imageSenders.add(address.toLowerCase());
		else imageSenders.delete(address.toLowerCase());
		return send(res, 200, { addresses: [...imageSenders] });
	}
	m = /^\/api\/messages\/(\d+)$/.exec(path);
	if (m) {
		const found = messageById(Number(m[1]));
		if (!found) return error(res, 404, 'not_found', 'no such message');
		const { row: r, message: msg } = found;
		await sleep(LATENCY_MS);
		return send(res, 200, {
			id: msg.id,
			threadId: r.id,
			accountId: r.accountId,
			subject: msg.subject,
			from: msg.from ? [msg.from] : [],
			to: msg.to,
			cc: msg.cc,
			replyTo: [],
			date: msg.date,
			messageId: `${msg.id}@mock.tagmail.test`,
			unread: msg.unread,
			starred: msg.starred,
			body: 'html',
			remoteImages: remoteImages(r, msg),
			imagesAllowed: !!msg.from && imageSenders.has(msg.from.email.toLowerCase()),
			hasInvitation: hasInvitation(msg.id),
			parts: partsOf(msg)
		});
	}
	m = /^\/api\/parts\/([^/]+)$/.exec(path);
	if (m) {
		res.writeHead(200, {
			'content-type': 'application/pdf',
			'content-disposition': 'attachment; filename="Statement-November.pdf"',
			'x-content-type-options': 'nosniff'
		});
		return res.end('%PDF-1.4\n%mock\n');
	}
	m = /^\/api\/messages\/(\d+)\/render$/.exec(path);
	if (m) {
		const found = messageById(Number(m[1]));
		if (!found) return error(res, 404, 'not_found', 'no such message');
		const images =
			url.searchParams.get('images') === '1' ||
			(!!found.message.from && imageSenders.has(found.message.from.email.toLowerCase()));
		const html = renderMessage(found.row, found.message, images, authorCss);
		res.writeHead(200, {
			'content-type': 'text/html; charset=utf-8',
			'cache-control': 'no-store',
			'x-content-type-options': 'nosniff',
			'content-security-policy': RENDER_CSP
		});
		return res.end(html);
	}
	if (path === '/api/events') {
		res.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-store' });
		res.write(': connected\n\n');
		sseClients.add(res);
		req.on('close', () => sseClients.delete(res));
		return;
	}
	return error(res, 404, 'not_found', 'no such route');
}

/** Test hooks: deliver new mail through the change feed. */
async function mock(req: IncomingMessage, res: ServerResponse, url: URL) {
	if (url.pathname === '/__mock/new-mail' && req.method === 'POST') {
		const { subject } = await body(req);
		return send(res, 200, { id: deliverNewMail(subject) });
	}
	if (url.pathname === '/__mock/sync-mode' && req.method === 'POST') {
		syncMode = ((await body(req)).mode ?? 'none') as SyncMode;
		return send(res, 204, '');
	}
	if (url.pathname === '/__mock/render-author-css' && req.method === 'POST') {
		authorCss = String((await body(req)).on) === 'true';
		return send(res, 204, '');
	}
	if (url.pathname === '/__mock/sync' && req.method === 'GET') {
		return send(res, 200, { calls: syncCalls, mode: syncMode });
	}
	if (url.pathname === '/__mock/scenario' && req.method === 'POST') {
		const { name } = await body(req);
		setScenario(name === 'multi' ? 'multi' : 'single');
		resetWrites();
		return send(res, 204, '');
	}
	if (url.pathname === '/__mock/reset' && req.method === 'POST') {
		setScenario('single');
		resetCaldav();
		resetDesktop();
		resetPush();
		ownerExists = true;
		sessions = initialSessions();
		devices = initialDevices();
		extraRows.clear();
		imageSenders.clear();
		extraOrder.length = 0;
		syncMode = 'none';
		authorCss = false;
		syncCalls.length = 0;
		resetWrites();
		return send(res, 204, '');
	}
	if (url.pathname === '/__mock/fresh-install' && req.method === 'POST') {
		ownerExists = false;
		return send(res, 204, '');
	}
	if (await desktopMock(req, res, url, writeContext)) return;
	if (await writeMock(req, res, url, writeContext)) return;
	if (await accountsMock(req, res, url, writeContext)) return;
	if (await calendarMock(req, res, url, writeContext)) return;
	if (await pushMock(req, res, url, writeContext)) return;
	return error(res, 404, 'not_found', 'no such mock hook');
}

const writeContext: WriteContext = {
	row,
	push: pushChanges,
	send,
	error,
	readBody
};

function serveStatic(res: ServerResponse, path: string) {
	let file = normalize(join(ROOT, decodeURIComponent(path)));
	if (!file.startsWith(ROOT)) return error(res, 400, 'bad_request', 'bad path');
	if (!existsSync(file) || statSync(file).isDirectory()) file = join(ROOT, 'index.html');
	const immutable = path.startsWith('/_app/immutable/');
	res.writeHead(200, {
		'content-type': TYPES[extname(file)] ?? 'application/octet-stream',
		'content-security-policy': WEB_APP_CSP,
		'cache-control': immutable ? 'public, max-age=31536000, immutable' : 'no-cache',
		'x-content-type-options': 'nosniff'
	});
	createReadStream(file).pipe(res);
}

createServer((req, res) => {
	const url = new URL(req.url ?? '/', `http://${req.headers.host ?? 'localhost'}`);
	const run = url.pathname.startsWith('/api/')
		? api(req, res, url)
		: url.pathname.startsWith('/__mock/')
			? mock(req, res, url)
			: Promise.resolve(serveStatic(res, url.pathname));
	run.catch((e: unknown) => {
		console.error(e);
		if (!res.headersSent) error(res, 500, 'internal', 'mock failure');
	});
}).listen(PORT, '127.0.0.1', () =>
	console.log(`mock API on http://127.0.0.1:${PORT} serving ${ROOT}`)
);
