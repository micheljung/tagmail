import type { IncomingMessage, ServerResponse } from 'node:http';

/** The desktop app's local routes, for specs that fake the Tauri globals. */
interface DesktopState {
	enabled: boolean;
	paired: boolean;
	online: boolean;
	phase: unknown;
	warnings: string[];
	settings: Record<string, unknown>;
}

const OFFLINE_OPEN = 'Older message needs a connection';

const initial = (): DesktopState => ({
	enabled: false,
	paired: true,
	online: true,
	phase: { kind: 'live' },
	warnings: [],
	settings: {
		windowDays: 90,
		rawCapMb: 2048,
		notify: true,
		notifyContent: true,
		reminders: true,
		autostart: false,
		closeToTray: true
	}
});

let state = initial();

export function resetDesktop(): void {
	state = initial();
}

function status() {
	return {
		paired: state.paired,
		server: state.paired ? 'https://mail.example.com' : null,
		online: state.online,
		phase: state.paired ? state.phase : { kind: 'unpaired' },
		lastSync: state.paired ? 1_795_000_000 : null,
		warnings: state.warnings
	};
}

export interface DesktopContext {
	send(res: ServerResponse, status: number, body: unknown, headers?: Record<string, string>): void;
	error(res: ServerResponse, status: number, code: string, message: string): void;
	readBody(req: IncomingMessage): Promise<Buffer>;
}

async function json(req: IncomingMessage, ctx: DesktopContext): Promise<Record<string, unknown>> {
	const text = (await ctx.readBody(req)).toString('utf8');
	return text ? (JSON.parse(text) as Record<string, unknown>) : {};
}

/** Answers `/api/desktop/*` and the desktop's own session; true when handled. */
export async function desktopApi(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: DesktopContext
): Promise<boolean> {
	if (!state.enabled) return false;
	const method = req.method ?? 'GET';
	const path = url.pathname;
	if (path === '/api/auth/session') {
		if (state.paired) ctx.send(res, 200, { via: 'device', session: null, deviceId: null });
		else ctx.error(res, 401, 'unpaired', 'pair this device with your server');
		return true;
	}
	if (path === '/api/desktop/status') ctx.send(res, 200, status());
	else if (path === '/api/desktop/events/start') ctx.send(res, 204, '');
	else if (path === '/api/desktop/pair' && method === 'POST') {
		const { code } = await json(req, ctx);
		if (code !== 'K7QD2XPM') ctx.error(res, 400, 'invalid_code', 'That code is wrong or expired');
		else {
			state.paired = true;
			ctx.send(res, 200, status());
		}
	} else if (path === '/api/desktop/unpair' && method === 'POST') {
		state.paired = false;
		ctx.send(res, 204, '');
	} else if (path === '/api/desktop/settings' && method === 'GET')
		ctx.send(res, 200, state.settings);
	else if (path === '/api/desktop/settings' && method === 'PATCH') {
		state.settings = { ...state.settings, ...(await json(req, ctx)) };
		ctx.send(res, 200, state.settings);
	} else if (!state.paired) ctx.error(res, 401, 'unpaired', 'pair this device with your server');
	else if (!state.online && /^\/api\/threads\/\d+$/.test(path)) {
		ctx.error(res, 503, 'offline', OFFLINE_OPEN);
	} else return false;
	return true;
}

/** Response headers the desktop adds to an API answer, e.g. offline search. */
export function desktopHeaders(view: string): Record<string, string> {
	return state.enabled && !state.online && view.startsWith('search:')
		? { 'x-tagmail-partial': 'offline' }
		: {};
}

/** `POST /__mock/desktop`: switch the desktop routes on and set their state. */
export async function desktopMock(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: DesktopContext
): Promise<boolean> {
	if (url.pathname !== '/__mock/desktop' || req.method !== 'POST') return false;
	state = { ...state, ...(await json(req, ctx)), enabled: true };
	ctx.send(res, 204, '');
	return true;
}
