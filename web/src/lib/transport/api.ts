import { decodeIdIndex } from '@tagmail/api-types';
import type {
	ApiErrorDto,
	ApiTransport,
	CurrentAuthDto,
	DeviceDto,
	DevicesDto,
	ImageSendersDto,
	LabelDto,
	MailboxDto,
	MessageDto,
	PairingCodeDto,
	PasskeyChallengeDto,
	PasskeyDto,
	PasskeysDto,
	SessionDto,
	SessionsDto,
	StatusDto,
	ThreadDetailDto,
	ThreadsDto
} from '@tagmail/api-types';

export const CURSOR_HEADER = 'x-tagmail-cursor';

export class ApiError extends Error {
	readonly status: number;
	readonly code: string;
	readonly field: string | undefined;

	constructor(status: number, body: ApiErrorDto) {
		super(body.message);
		this.status = status;
		this.code = body.error;
		this.field = body.field;
	}
}

export interface ViewIndexResult {
	ids: Float64Array;
	cursor: string;
}

/** A list view id as sent to `/api/views/{view}/index`. */
export type ViewId = 'inbox' | `mailbox:${number}` | `label:${string}` | `search:${string}`;

async function errorOf(resp: Response): Promise<ApiError> {
	let body: ApiErrorDto;
	try {
		body = (await resp.json()) as ApiErrorDto;
	} catch {
		body = { error: 'http_' + resp.status, message: resp.statusText || `HTTP ${resp.status}` };
	}
	return new ApiError(resp.status, body);
}

export class Api {
	readonly transport: ApiTransport;
	/** Called on every 401 so the shell can send the player to the login page. */
	onUnauthorized: (error: ApiError) => void = () => {};

	constructor(transport: ApiTransport) {
		this.transport = transport;
	}

	async #send(path: string, init?: RequestInit): Promise<Response> {
		const resp = await this.transport.request(path, init);
		if (resp.ok) return resp;
		const err = await errorOf(resp);
		if (resp.status === 401 && !path.startsWith('/api/auth/')) this.onUnauthorized(err);
		throw err;
	}

	async #json<T>(path: string, init?: RequestInit): Promise<T> {
		return (await (await this.#send(path, init)).json()) as T;
	}

	#post<T>(path: string, body?: unknown, method = 'POST'): Promise<T> {
		return this.#json<T>(path, {
			method,
			headers: body === undefined ? undefined : { 'content-type': 'application/json' },
			body: body === undefined ? undefined : JSON.stringify(body)
		});
	}

	async #empty(path: string, method: string, body?: unknown): Promise<void> {
		await this.#send(path, {
			method,
			headers: body === undefined ? undefined : { 'content-type': 'application/json' },
			body: body === undefined ? undefined : JSON.stringify(body)
		});
	}

	async viewIndex(view: ViewId, signal?: AbortSignal): Promise<ViewIndexResult> {
		const resp = await this.#send(`/api/views/${encodeURIComponent(view)}/index`, { signal });
		const bytes = new Uint8Array(await resp.arrayBuffer());
		return {
			ids: Float64Array.from(decodeIdIndex(bytes)),
			cursor: resp.headers.get(CURSOR_HEADER) ?? ''
		};
	}

	threads(ids: readonly number[], signal?: AbortSignal): Promise<ThreadsDto> {
		return this.#json<ThreadsDto>(`/api/threads?ids=${ids.join(',')}`, { signal });
	}

	thread(id: number, signal?: AbortSignal): Promise<ThreadDetailDto> {
		return this.#json<ThreadDetailDto>(`/api/threads/${id}`, { signal });
	}

	message(id: number, signal?: AbortSignal): Promise<MessageDto> {
		return this.#json<MessageDto>(`/api/messages/${id}`, { signal });
	}

	/** Always (or no longer) load remote images from `address`. */
	setImageSender(address: string, allow: boolean): Promise<ImageSendersDto> {
		return this.#post<ImageSendersDto>('/api/image-senders', { address, allow });
	}

	mailboxes(): Promise<MailboxDto[]> {
		return this.#json<MailboxDto[]>('/api/mailboxes');
	}

	labels(): Promise<LabelDto[]> {
		return this.#json<LabelDto[]>('/api/labels');
	}

	status(): Promise<StatusDto> {
		return this.#json<StatusDto>('/api/status');
	}

	session(): Promise<CurrentAuthDto> {
		return this.#json<CurrentAuthDto>('/api/auth/session');
	}

	login(password: string): Promise<CurrentAuthDto> {
		return this.#post<CurrentAuthDto>('/api/auth/login', { password });
	}

	setup(token: string, password: string): Promise<CurrentAuthDto> {
		return this.#post<CurrentAuthDto>('/api/auth/setup', { token, password });
	}

	logout(): Promise<void> {
		return this.#empty('/api/auth/logout', 'POST');
	}

	passkeyLoginStart(): Promise<PasskeyChallengeDto> {
		return this.#post<PasskeyChallengeDto>('/api/auth/passkey/login/start');
	}

	passkeyLoginFinish(ceremonyId: string, credential: unknown): Promise<CurrentAuthDto> {
		return this.#post<CurrentAuthDto>('/api/auth/passkey/login/finish', { ceremonyId, credential });
	}

	passkeyRegisterStart(name: string): Promise<PasskeyChallengeDto> {
		return this.#post<PasskeyChallengeDto>('/api/auth/passkey/register/start', { name });
	}

	passkeyRegisterFinish(ceremonyId: string, credential: unknown): Promise<PasskeyDto> {
		return this.#post<PasskeyDto>('/api/auth/passkey/register/finish', { ceremonyId, credential });
	}

	async passkeys(): Promise<PasskeyDto[]> {
		return (await this.#json<PasskeysDto>('/api/passkeys')).passkeys;
	}

	removePasskey(id: number): Promise<void> {
		return this.#empty(`/api/passkeys/${id}`, 'DELETE');
	}

	async sessions(): Promise<SessionDto[]> {
		return (await this.#json<SessionsDto>('/api/sessions')).sessions;
	}

	revokeSession(id: string): Promise<void> {
		return this.#empty(`/api/sessions/${encodeURIComponent(id)}`, 'DELETE');
	}

	async devices(): Promise<DeviceDto[]> {
		return (await this.#json<DevicesDto>('/api/devices')).devices;
	}

	revokeDevice(id: number): Promise<void> {
		return this.#empty(`/api/devices/${id}`, 'DELETE');
	}

	startPairing(): Promise<PairingCodeDto> {
		return this.#post<PairingCodeDto>('/api/pairing');
	}
}

export function renderUrl(messageId: number, images: boolean): string {
	return `/api/messages/${messageId}/render?images=${images ? 1 : 0}`;
}

export function downloadUrl(partUrl: string): string {
	return partUrl + (partUrl.includes('?') ? '&' : '?') + 'download=1';
}
