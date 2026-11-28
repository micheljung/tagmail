import type { IncomingMessage, ServerResponse } from 'node:http';
import type {
	PushSubscribeDto,
	PushSubscriptionDto,
	PushSubscriptionPatchDto,
	ReminderSettingsPatchDto
} from '../../../packages/api-types/src/index.ts';
import { accountState } from './accounts.ts';
import type { WriteContext } from './write.ts';

/** Mock of `/api/push/*`; the key is a real P-256 point so browsers accept it. */

export const VAPID_PUBLIC_KEY =
	'BNkLAZfpKyyDmoAp7pfoOAtOqZULZJPsLPXX_v3a0ITW5pyENjLvEG9pMib3D0hsah51sh3ipBhECiP28ZwelTI';
const T0 = 1_795_000_000;
const OTHER_DEVICE_UA =
	'Mozilla/5.0 (Linux; Android 15; Pixel 7) AppleWebKit/537.36 Chrome/147.0 Mobile Safari/537.36';

interface Row extends PushSubscriptionDto {
	endpoint: string;
}

let rows: Row[] = [];
let nextId = 1;
/** Request bodies of `POST /api/push/subscriptions`, oldest first. */
let posted: PushSubscribeDto[] = [];
let tests: number[] = [];
/** Alarm keys `POST /api/reminders/{key}/dismiss` was called with. */
let dismissed: string[] = [];
let timeZone: string | null = null;

export function resetPush(): void {
	rows = [];
	nextId = 1;
	posted = [];
	tests = [];
	dismissed = [];
	timeZone = null;
}

/** Like a server before fix #2, which refused a POST naming a removed account. */
const UNKNOWN_ACCOUNTS = { error: 'invalid', message: 'unknown accounts', field: 'accountIds' };

function unknownAccounts(ids: number[] | null | undefined): boolean {
	const known = new Set(accountState().accounts.map((a) => a.id));
	return (ids ?? []).some((id) => !known.has(id));
}

const dto = (row: Row): PushSubscriptionDto => {
	const { endpoint, ...rest } = row;
	void endpoint;
	return rest;
};

export async function pushApi(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	const method = req.method ?? 'GET';
	const path = url.pathname;
	if (!path.startsWith('/api/push/') && !path.startsWith('/api/reminders/')) return false;
	const json = async <T>() => JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as T;
	const ok = (status: number, body: unknown) => {
		ctx.send(res, status, body);
		return true;
	};

	if (path === '/api/reminders/settings' && method === 'GET') return ok(200, { timeZone });
	if (path === '/api/reminders/settings' && method === 'PATCH') {
		const body = await json<ReminderSettingsPatchDto>();
		if (!/^[A-Za-z_]+(\/[A-Za-z0-9_+-]+)+$|^UTC$/.test(body.timeZone ?? ''))
			return ok(422, { error: 'invalid', message: 'not a time zone', field: 'timeZone' });
		if (!(body.ifUnset && timeZone !== null)) timeZone = body.timeZone;
		return ok(200, { timeZone });
	}
	const dismiss = /^\/api\/reminders\/([0-9a-f]+)\/dismiss$/.exec(path);
	if (dismiss && method === 'POST') {
		await ctx.readBody(req);
		dismissed.push(dismiss[1]);
		ctx.send(res, 204, '');
		return true;
	}
	if (!path.startsWith('/api/push/')) return false;

	if (path === '/api/push/key' && method === 'GET') return ok(200, { publicKey: VAPID_PUBLIC_KEY });
	if (path === '/api/push/subscriptions' && method === 'GET') return ok(200, rows.map(dto));
	if (path === '/api/push/subscriptions' && method === 'POST') {
		const body = await json<PushSubscribeDto>();
		posted.push(body);
		if (!body.endpoint?.startsWith('https://'))
			return ok(422, { error: 'invalid', message: 'bad endpoint', field: 'endpoint' });
		if (!body.keys?.p256dh || !body.keys?.auth)
			return ok(422, { error: 'invalid', message: 'bad keys', field: 'keys.p256dh' });
		if (unknownAccounts(body.accountIds)) return ok(422, UNKNOWN_ACCOUNTS);
		const old = body.oldEndpoint
			? rows.find((r) => r.endpoint === body.oldEndpoint && r.endpoint !== body.endpoint)
			: undefined;
		if (old) {
			rows = rows.filter((r) => r !== old);
			body.preview ??= old.preview;
			if (body.accountIds === undefined && old.accountIds !== null)
				body.accountIds = old.accountIds;
			body.reminders ??= old.reminders;
		}
		const known = rows.find((r) => r.endpoint === body.endpoint);
		if (known) {
			known.current = true;
			if (body.preview !== undefined) known.preview = body.preview;
			if (body.accountIds !== undefined) known.accountIds = body.accountIds;
			if (body.reminders !== undefined) known.reminders = body.reminders;
			return ok(200, dto(known));
		}
		const row: Row = {
			id: nextId++,
			endpoint: body.endpoint,
			endpointHost: new URL(body.endpoint).host,
			preview: body.preview ?? true,
			accountIds: body.accountIds ?? null,
			userAgent: req.headers['user-agent'] ?? null,
			createdAt: T0,
			lastSuccessAt: null,
			current: true,
			reminders: body.reminders ?? false
		};
		rows.push(row);
		return ok(201, dto(row));
	}
	const match = /^\/api\/push\/subscriptions\/(\d+)(\/test)?$/.exec(path);
	const row = match ? rows.find((r) => r.id === Number(match[1])) : undefined;
	if (!match) return false;
	if (!row) return ok(404, { error: 'not_found', message: 'no such subscription' });
	if (match[2] && method === 'POST') {
		tests.push(row.id);
		ctx.send(res, 202, '');
		return true;
	}
	if (method === 'PATCH') {
		const patch = await json<PushSubscriptionPatchDto>();
		if (unknownAccounts(patch.accountIds)) return ok(422, UNKNOWN_ACCOUNTS);
		if (patch.preview !== undefined) row.preview = patch.preview;
		if (patch.accountIds !== undefined) row.accountIds = patch.accountIds;
		if (patch.reminders !== undefined) row.reminders = patch.reminders;
		return ok(200, dto(row));
	}
	if (method === 'DELETE') {
		rows = rows.filter((r) => r !== row);
		ctx.send(res, 204, '');
		return true;
	}
	return false;
}

/** `/__mock/push/*`: inspect what the app posted and add another device. */
export async function pushMock(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	if (url.pathname === '/__mock/push' && req.method === 'GET') {
		ctx.send(res, 200, { posted, tests, subscriptions: rows, dismissed, timeZone });
		return true;
	}
	if (url.pathname === '/__mock/push/other-device' && req.method === 'POST') {
		await ctx.readBody(req);
		rows.push({
			id: nextId++,
			endpoint: `https://fcm.googleapis.com/fcm/send/other-${nextId}`,
			endpointHost: 'fcm.googleapis.com',
			preview: true,
			accountIds: null,
			userAgent: OTHER_DEVICE_UA,
			createdAt: T0,
			lastSuccessAt: T0 + 60,
			current: false,
			reminders: false
		});
		ctx.send(res, 204, '');
		return true;
	}
	return false;
}
