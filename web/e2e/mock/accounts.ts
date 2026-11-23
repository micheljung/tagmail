import type { IncomingMessage, ServerResponse } from 'node:http';
import type {
	AccountDto,
	AccountPatchDto,
	AccountStatusDto,
	AccountTestDto,
	IdentityDto,
	IdentityPatchDto,
	NewAccountDto,
	NewIdentityDto
} from '../../../packages/api-types/src/index.ts';
import { ACCOUNT_B, B_ALIAS, B_ME, ME, MAILBOXES, B_MAILBOXES } from './fixture.ts';
import type { Change, WriteContext } from './write.ts';

/** Mock of the M4 account and identity API; `multi` adds a second account. */

export const ACCOUNT_A = 1;
export const IDENTITY_A1 = 1;
export const IDENTITY_B1 = 11;
export const IDENTITY_B2 = 12;
/** Hosts the account test treats as unreachable. */
export const UNREACHABLE_HOST = 'unreachable.example';
const TEST_MS = 42;
const SIGNATURE_MAX = 16 * 1024;
const MOCK_CAPABILITIES = ['IMAP4rev1', 'IDLE', 'CONDSTORE', 'QRESYNC', 'METADATA'];

let multi = false;
let accounts: AccountDto[] = [];
let identities: IdentityDto[] = [];
const degraded = new Map<number, string[]>();
const tests: { imapHost: string; username: string }[] = [];
let nextAccount = 3;
let nextIdentity = 20;

function account(id: number, name: string, email: string, host: string): AccountDto {
	return {
		id,
		name,
		email,
		status: 'active',
		imapHost: `imap.${host}`,
		imapPort: 993,
		imapTls: 'tls',
		smtpHost: `smtp.${host}`,
		smtpPort: 465,
		smtpTls: 'tls',
		username: email,
		maxConnections: 4
	};
}

export function isMulti(): boolean {
	return multi;
}

export function setScenario(name: 'single' | 'multi'): void {
	multi = name === 'multi';
	accounts = [account(ACCOUNT_A, 'Personal', ME.email, 'tagmail.test')];
	identities = [
		{
			id: IDENTITY_A1,
			accountId: ACCOUNT_A,
			address: ME.email,
			name: ME.name,
			signature: multi ? '<b>Michel</b><br>Personal' : null,
			isDefault: true
		},
		{
			id: 2,
			accountId: ACCOUNT_A,
			address: 'michel@work.example',
			name: ME.name,
			signature: null,
			isDefault: false
		}
	];
	if (multi) {
		accounts.push(account(ACCOUNT_B, 'Bergmail', B_ME.email, 'bergmail.example'));
		identities.push(
			{
				id: IDENTITY_B1,
				accountId: ACCOUNT_B,
				address: B_ME.email,
				name: B_ME.name,
				signature: 'Michel Jung<br>Bergmail',
				isDefault: true
			},
			{
				id: IDENTITY_B2,
				accountId: ACCOUNT_B,
				address: B_ALIAS.email,
				name: B_ALIAS.name,
				signature: '<i>The Berg team</i><br>team@bergmail.example',
				isDefault: false
			}
		);
	}
	degraded.clear();
	tests.length = 0;
	nextAccount = 3;
	nextIdentity = 20;
}
setScenario('single');

export function currentIdentities(): IdentityDto[] {
	return identities;
}

export function accountState() {
	return { accounts, identities, accountTests: tests };
}

export function mailboxes() {
	return multi ? [...MAILBOXES, ...B_MAILBOXES] : MAILBOXES;
}

/** The identity a reply from `accountId` to `recipients` is sent from. */
export function replyIdentity(accountId: number, recipients: { email: string }[]): IdentityDto {
	const own = identities.filter((i) => i.accountId === accountId);
	for (const r of recipients) {
		const hit = own.find((i) => i.address.toLowerCase() === r.email.toLowerCase());
		if (hit) return hit;
	}
	return own.find((i) => i.isDefault) ?? own[0];
}

function status(a: AccountDto): AccountStatusDto {
	const codes = [...(degraded.get(a.id) ?? [])];
	return {
		id: a.id,
		name: a.name,
		email: a.email,
		status: a.status,
		phase: a.status === 'active' ? 'live' : 'stopped',
		mailboxesTotal: mailboxes().filter((m) => m.accountId === a.id).length,
		mailboxesLive: mailboxes().filter((m) => m.accountId === a.id).length,
		textPending: 0,
		lastError: null,
		capabilities: MOCK_CAPABILITIES,
		degraded: codes
	};
}

function testResult(host: string): AccountTestDto {
	const down = host === UNREACHABLE_HOST;
	return {
		imap: {
			ok: !down,
			error: down ? `connect to ${host}:993: connection refused` : null,
			ms: TEST_MS
		},
		smtp: { ok: true, error: null, ms: TEST_MS },
		capabilities: down ? [] : MOCK_CAPABILITIES,
		mailboxes: down ? null : MAILBOXES.length
	};
}

const ADDRESS = /^[^\s@]+@[^\s@]+\.[^\s@]+$/;

function accountChange(id: number, payload: AccountDto | null): Change {
	return { entity: 'account', id, op: { kind: payload ? 'upsert' : 'delete' }, payload };
}

function identityChange(id: number, payload: IdentityDto | null): Change {
	return { entity: 'identity', id, op: { kind: payload ? 'upsert' : 'delete' }, payload };
}

function cleanSignature(html: string | null | undefined): string | null {
	if (!html) return null;
	return html.replace(/<script\b[\s\S]*?<\/script\s*>/gi, '').slice(0, SIGNATURE_MAX);
}

/** Handles an account, identity or status route; returns false when `path` is not one. */
export async function accountsApi(
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
	const fail = (status: number, code: string, message: string, field?: string) => {
		ctx.send(res, status, field ? { error: code, message, field } : { error: code, message });
		return true;
	};

	if (path === '/api/status') return ok(200, { accounts: accounts.map(status) });
	if (path === '/api/accounts' && method === 'GET') return ok(200, { accounts });
	if (path === '/api/accounts/test' && method === 'POST') {
		const b = await json<NewAccountDto>();
		tests.push({ imapHost: b.imapHost, username: b.username });
		return ok(200, testResult(b.imapHost));
	}
	if (path === '/api/accounts' && method === 'POST') {
		const b = await json<NewAccountDto>();
		const email = b.email?.trim() || (ADDRESS.test(b.username) ? b.username : '');
		if (!ADDRESS.test(email)) {
			return fail(
				422,
				'invalid',
				'the IMAP username is not an email address; enter the address to send from',
				'email'
			);
		}
		const created: AccountDto = {
			...account(nextAccount++, b.name, email.toLowerCase(), 'example'),
			imapHost: b.imapHost,
			imapPort: b.imapPort,
			imapTls: b.imapTls,
			smtpHost: b.smtpHost,
			smtpPort: b.smtpPort,
			smtpTls: b.smtpTls,
			username: b.username,
			maxConnections: b.maxConnections ?? 4
		};
		const identity: IdentityDto = {
			id: nextIdentity++,
			accountId: created.id,
			address: created.email!,
			name: b.displayName ?? null,
			signature: null,
			isDefault: true
		};
		accounts.push(created);
		identities.push(identity);
		ctx.push([accountChange(created.id, created), identityChange(identity.id, identity)]);
		return ok(201, created);
	}
	let m = /^\/api\/accounts\/(\d+)(\/test)?$/.exec(path);
	if (m) {
		const a = accounts.find((x) => x.id === Number(m![1]));
		if (!a) return fail(404, 'not_found', 'no such account');
		if (m[2] && method === 'POST') {
			const b = await json<AccountPatchDto>();
			tests.push({ imapHost: b.imapHost ?? a.imapHost, username: b.username ?? a.username });
			return ok(200, testResult(b.imapHost ?? a.imapHost));
		}
		if (method === 'PATCH') {
			const patch = await json<AccountPatchDto>();
			delete patch.password;
			Object.assign(a, patch);
			ctx.push([accountChange(a.id, a)]);
			return ok(200, a);
		}
		if (method === 'DELETE') {
			accounts = accounts.filter((x) => x !== a);
			identities = identities.filter((i) => i.accountId !== a.id);
			ctx.push([accountChange(a.id, null)]);
			return ok(202, { id: a.id });
		}
		return ok(200, a);
	}

	if (path === '/api/identities' && method === 'GET') {
		const order = (i: IdentityDto) => [i.accountId, i.isDefault ? 0 : 1, i.id];
		const sorted = [...identities].sort((x, y) => {
			const [a, b] = [order(x), order(y)];
			return a[0] - b[0] || a[1] - b[1] || a[2] - b[2];
		});
		return ok(200, { identities: sorted });
	}
	if (path === '/api/identities' && method === 'POST') {
		const b = await json<NewIdentityDto>();
		if (!ADDRESS.test(b.address ?? '')) {
			return fail(422, 'invalid', 'not an email address', 'address');
		}
		if (
			identities.some(
				(i) => i.accountId === b.accountId && i.address.toLowerCase() === b.address.toLowerCase()
			)
		) {
			return fail(409, 'conflict', 'that address is already an identity of the account');
		}
		const identity: IdentityDto = {
			id: nextIdentity++,
			accountId: b.accountId,
			address: b.address,
			name: b.name ?? null,
			signature: cleanSignature(b.signature),
			isDefault: false
		};
		identities.push(identity);
		const changes = [identityChange(identity.id, identity)];
		if (b.isDefault) changes.push(...makeDefault(identity));
		ctx.push(changes);
		return ok(201, identity);
	}
	m = /^\/api\/identities\/(\d+)$/.exec(path);
	if (m) {
		const identity = identities.find((i) => i.id === Number(m[1]));
		if (!identity) return fail(404, 'not_found', 'no such identity');
		if (method === 'PATCH') {
			const b = await json<IdentityPatchDto>();
			if (b.address !== undefined && b.address !== null) identity.address = b.address;
			if (b.name !== undefined) identity.name = b.name || null;
			if (b.signature !== undefined) identity.signature = cleanSignature(b.signature);
			const changes = [identityChange(identity.id, identity)];
			if (b.isDefault) changes.push(...makeDefault(identity));
			ctx.push(changes);
			return ok(200, identity);
		}
		if (method === 'DELETE') {
			const own = identities.filter((i) => i.accountId === identity.accountId);
			if (own.length === 1) {
				return fail(409, 'conflict', "an account's last identity can't be deleted");
			}
			identities = identities.filter((i) => i !== identity);
			const changes = [identityChange(identity.id, null)];
			if (identity.isDefault) {
				const next = own.filter((i) => i !== identity).sort((a, b) => a.id - b.id)[0];
				changes.push(...makeDefault(next));
			}
			ctx.push(changes);
			return ok(204, '');
		}
	}
	return false;
}

function makeDefault(identity: IdentityDto): Change[] {
	const changes: Change[] = [];
	for (const i of identities) {
		if (i.accountId !== identity.accountId) continue;
		const next = i === identity;
		if (i.isDefault !== next) {
			i.isDefault = next;
			changes.push(identityChange(i.id, i));
		}
	}
	return changes;
}

/** Test hooks. */
export async function accountsMock(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	if (url.pathname === '/__mock/degraded' && req.method === 'POST') {
		const text = (await ctx.readBody(req)).toString('utf8');
		const { accountId, codes } = JSON.parse(text) as { accountId: number; codes: string[] };
		degraded.set(accountId, codes);
		const a = accounts.find((x) => x.id === accountId);
		if (a) ctx.push([accountChange(a.id, a)]);
		ctx.send(res, 204, '');
		return true;
	}
	return false;
}
