import type { IncomingMessage, ServerResponse } from 'node:http';
import type {
	PushSubscribeDto,
	PushSubscriptionDto,
	PushSubscriptionPatchDto
} from '../../../packages/api-types/src/index.ts';
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

export function resetPush(): void {
	rows = [];
	nextId = 1;
	posted = [];
	tests = [];
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
	if (!path.startsWith('/api/push/')) return false;
	const json = async <T>() => JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as T;
	const ok = (status: number, body: unknown) => {
		ctx.send(res, status, body);
		return true;
	};

	if (path === '/api/push/key' && method === 'GET') return ok(200, { publicKey: VAPID_PUBLIC_KEY });
	if (path === '/api/push/subscriptions' && method === 'GET') return ok(200, rows.map(dto));
	if (path === '/api/push/subscriptions' && method === 'POST') {
		const body = await json<PushSubscribeDto>();
		posted.push(body);
		if (!body.endpoint?.startsWith('https://'))
			return ok(422, { error: 'invalid', message: 'bad endpoint', field: 'endpoint' });
		if (!body.keys?.p256dh || !body.keys?.auth)
			return ok(422, { error: 'invalid', message: 'bad keys', field: 'keys.p256dh' });
		const known = rows.find((r) => r.endpoint === body.endpoint);
		if (known) {
			known.current = true;
			if (body.preview !== undefined) known.preview = body.preview;
			if (body.accountIds !== undefined) known.accountIds = body.accountIds;
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
			current: true
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
		if (patch.preview !== undefined) row.preview = patch.preview;
		if (patch.accountIds !== undefined) row.accountIds = patch.accountIds;
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
		ctx.send(res, 200, { posted, tests, subscriptions: rows });
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
			current: false
		});
		ctx.send(res, 204, '');
		return true;
	}
	return false;
}
