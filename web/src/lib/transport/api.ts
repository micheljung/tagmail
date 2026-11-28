import { decodeIdIndex } from '@tagmail/api-types';
import type {
	AccountDto,
	AccountPatchDto,
	AccountsDto,
	AccountTestDto,
	ApiErrorDto,
	ApiTransport,
	CalApplyDto,
	CaldavDiscoveryDto,
	CaldavSettingsDto,
	CaldavSettingsInputDto,
	CalendarDto,
	CalendarEventsDto,
	CalendarPatchDto,
	CalendarTargetDto,
	CalendarsDto,
	CalImportDto,
	CalRsvpDto,
	ConfirmHostDto,
	CurrentAuthDto,
	DesktopPairDto,
	DesktopSettingsDto,
	DesktopSettingsPatchDto,
	DesktopStatusDto,
	DeviceDto,
	DevicesDto,
	EventInstanceDto,
	ImageSendersDto,
	InvitationDto,
	LabelDto,
	MailboxDto,
	MessageDto,
	PairingCodeDto,
	PasskeyChallengeDto,
	PasskeyDto,
	PasskeysDto,
	PushKeyDto,
	PushSubscribeDto,
	PushSubscriptionDto,
	PushSubscriptionPatchDto,
	SessionDto,
	SessionsDto,
	StatusDto,
	ThreadDetailDto,
	ThreadsDto
} from '@tagmail/api-types';
import { CSRF_HEADER, CSRF_VALUE } from './http';
import type {
	AttachmentDto,
	DraftDto,
	DraftInputDto,
	IdentitiesDto,
	IdentityDto,
	IdentityPatchDto,
	LabelChangeDto,
	LabelExportDto,
	LabelImportResultDto,
	LabelJobDto,
	LabelJobsDto,
	LabelPatchDto,
	NewAccountDto,
	NewIdentityDto,
	NewLabelDto,
	NewOpDto,
	OpDto,
	OpsDto,
	OutboxDto,
	OutboxItemDto,
	ReplyDraftDto,
	ReplyMode,
	SendDto,
	SendStatusDto
} from '@tagmail/api-types';

export const CURSOR_HEADER = 'x-tagmail-cursor';
/** `offline` when the desktop answered a search from local mail only. */
export const PARTIAL_HEADER = 'x-tagmail-partial';

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
	/** Offline desktop search: older results need a connection. */
	partial: boolean;
}

/** A view that spans every account unless scoped to one. */
export type ScopableView = 'inbox' | `label:${string}` | `search:${string}`;

/** A list view id as sent to `/api/views/{view}/index`. */
export type ViewId = ScopableView | `mailbox:${number}` | `account:${number}/${ScopableView}`;

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
	/** Called when the desktop app could not reach the server for a request. */
	onOffline: (error: ApiError) => void = () => {};
	/** Runs before logout and must settle quickly; its failure never blocks logout. */
	beforeLogout: () => Promise<void> = () => Promise.resolve();

	constructor(transport: ApiTransport) {
		this.transport = transport;
	}

	async #send(path: string, init?: RequestInit): Promise<Response> {
		const resp = await this.transport.request(path, init);
		if (resp.ok) return resp;
		const err = await errorOf(resp);
		if (resp.status === 401 && !path.startsWith('/api/auth/')) this.onUnauthorized(err);
		if (resp.status === 503 && err.code === 'offline') this.onOffline(err);
		throw err;
	}

	async #json<T>(path: string, init?: RequestInit): Promise<T> {
		return (await (await this.#send(path, init)).json()) as T;
	}

	#post<T>(path: string, body?: unknown, method = 'POST', keepalive = false): Promise<T> {
		return this.#json<T>(path, {
			method,
			keepalive,
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
			cursor: resp.headers.get(CURSOR_HEADER) ?? '',
			partial: resp.headers.get(PARTIAL_HEADER) === 'offline'
		};
	}

	/** Asks the engine to sync the view's accounts now (`202`, no completion signal). */
	syncView(view: ViewId): Promise<void> {
		return this.#empty(`/api/views/${encodeURIComponent(view)}/sync`, 'POST');
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

	invitation(id: number, signal?: AbortSignal): Promise<InvitationDto> {
		return this.#json<InvitationDto>(`/api/messages/${id}/invitation`, { signal });
	}

	async calendars(signal?: AbortSignal): Promise<CalendarDto[]> {
		return (await this.#json<CalendarsDto>('/api/calendars', { signal })).calendars;
	}

	/** Instances overlapping `[from, to)` (Unix seconds); no `calendars` lists every calendar. */
	async calendarEvents(
		from: number,
		to: number,
		calendars: readonly CalendarTargetDto[],
		signal?: AbortSignal
	): Promise<EventInstanceDto[]> {
		const query = new URLSearchParams({ from: String(from), to: String(to) });
		for (const c of calendars) query.append('calendars', `${c.accountId}:${c.calendarHref}`);
		return (await this.#json<CalendarEventsDto>(`/api/calendar/events?${query}`, { signal }))
			.events;
	}

	rsvp(request: CalRsvpDto): Promise<OpDto> {
		return this.#post<OpDto>('/api/calendar/rsvp', request);
	}

	importInvitation(request: CalImportDto): Promise<OpDto> {
		return this.#post<OpDto>('/api/calendar/import', request);
	}

	applyInvitation(request: CalApplyDto): Promise<OpDto> {
		return this.#post<OpDto>('/api/calendar/apply', request);
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

	desktopStatus(): Promise<DesktopStatusDto> {
		return this.#json<DesktopStatusDto>('/api/desktop/status');
	}

	pair(pairing: DesktopPairDto): Promise<DesktopStatusDto> {
		return this.#post<DesktopStatusDto>('/api/desktop/pair', pairing);
	}

	unpair(): Promise<void> {
		return this.#empty('/api/desktop/unpair', 'POST');
	}

	desktopSettings(): Promise<DesktopSettingsDto> {
		return this.#json<DesktopSettingsDto>('/api/desktop/settings');
	}

	updateDesktopSettings(patch: DesktopSettingsPatchDto): Promise<DesktopSettingsDto> {
		return this.#post<DesktopSettingsDto>('/api/desktop/settings', patch, 'PATCH');
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

	async logout(): Promise<void> {
		await this.beforeLogout().catch(() => {});
		return this.#empty('/api/auth/logout', 'POST');
	}

	pushKey(): Promise<PushKeyDto> {
		return this.#json<PushKeyDto>('/api/push/key');
	}

	pushSubscriptions(): Promise<PushSubscriptionDto[]> {
		return this.#json<PushSubscriptionDto[]>('/api/push/subscriptions');
	}

	subscribePush(subscription: PushSubscribeDto): Promise<PushSubscriptionDto> {
		return this.#post<PushSubscriptionDto>('/api/push/subscriptions', subscription);
	}

	updatePushSubscription(
		id: number,
		patch: PushSubscriptionPatchDto
	): Promise<PushSubscriptionDto> {
		return this.#post<PushSubscriptionDto>(`/api/push/subscriptions/${id}`, patch, 'PATCH');
	}

	removePushSubscription(id: number): Promise<void> {
		return this.#empty(`/api/push/subscriptions/${id}`, 'DELETE');
	}

	testPush(id: number): Promise<void> {
		return this.#empty(`/api/push/subscriptions/${id}/test`, 'POST');
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

	postOp(op: NewOpDto): Promise<OpDto> {
		return this.#post<OpDto>('/api/ops', op);
	}

	async ops(status: 'pending' | 'failed'): Promise<OpDto[]> {
		return (await this.#json<OpsDto>(`/api/ops?status=${status}`)).ops;
	}

	retryOp(opId: string): Promise<OpDto> {
		return this.#post<OpDto>(`/api/ops/${encodeURIComponent(opId)}/retry`);
	}

	discardOp(opId: string): Promise<void> {
		return this.#empty(`/api/ops/${encodeURIComponent(opId)}`, 'DELETE');
	}

	createLabel(label: NewLabelDto): Promise<LabelDto> {
		return this.#post<LabelDto>('/api/labels', label);
	}

	/** A new name may change the key (`label.key` of the answer); `color: ''` clears it. */
	updateLabel(key: string, change: LabelPatchDto): Promise<LabelChangeDto> {
		return this.#post<LabelChangeDto>(`/api/labels/${encodeURIComponent(key)}`, change, 'PATCH');
	}

	deleteLabel(key: string): Promise<LabelChangeDto> {
		return this.#post<LabelChangeDto>(
			`/api/labels/${encodeURIComponent(key)}`,
			undefined,
			'DELETE'
		);
	}

	async labelJobs(): Promise<LabelJobDto[]> {
		return (await this.#json<LabelJobsDto>('/api/label-jobs')).jobs;
	}

	async identities(): Promise<IdentityDto[]> {
		return (await this.#json<IdentitiesDto>('/api/identities')).identities;
	}

	createIdentity(identity: NewIdentityDto): Promise<IdentityDto> {
		return this.#post<IdentityDto>('/api/identities', identity);
	}

	/** `signature: ''` clears it. */
	updateIdentity(id: number, change: IdentityPatchDto): Promise<IdentityDto> {
		return this.#post<IdentityDto>(`/api/identities/${id}`, change, 'PATCH');
	}

	deleteIdentity(id: number): Promise<void> {
		return this.#empty(`/api/identities/${id}`, 'DELETE');
	}

	async accounts(): Promise<AccountDto[]> {
		return (await this.#json<AccountsDto>('/api/accounts')).accounts;
	}

	createAccount(account: NewAccountDto): Promise<AccountDto> {
		return this.#post<AccountDto>('/api/accounts', account);
	}

	/** Connects with unsaved settings; nothing is stored. */
	testNewAccount(account: NewAccountDto): Promise<AccountTestDto> {
		return this.#post<AccountTestDto>('/api/accounts/test', account);
	}

	/** Connects with `change` applied over the stored settings; nothing is stored. */
	testAccount(id: number, change: AccountPatchDto): Promise<AccountTestDto> {
		return this.#post<AccountTestDto>(`/api/accounts/${id}/test`, change);
	}

	updateAccount(id: number, change: AccountPatchDto): Promise<AccountDto> {
		return this.#post<AccountDto>(`/api/accounts/${id}`, change, 'PATCH');
	}

	deleteAccount(id: number): Promise<void> {
		return this.#empty(`/api/accounts/${id}`, 'DELETE');
	}

	caldavSettings(accountId: number): Promise<CaldavSettingsDto> {
		return this.#json<CaldavSettingsDto>(`/api/accounts/${accountId}/caldav`);
	}

	saveCaldavSettings(accountId: number, input: CaldavSettingsInputDto): Promise<CaldavSettingsDto> {
		return this.#post<CaldavSettingsDto>(`/api/accounts/${accountId}/caldav`, input, 'PUT');
	}

	discoverCaldav(accountId: number): Promise<CaldavDiscoveryDto> {
		return this.#post<CaldavDiscoveryDto>(`/api/accounts/${accountId}/caldav/discover`);
	}

	confirmCaldavHost(accountId: number, host: string): Promise<CaldavDiscoveryDto> {
		return this.#post<CaldavDiscoveryDto>(`/api/accounts/${accountId}/caldav/confirm-host`, {
			host
		} satisfies ConfirmHostDto);
	}

	updateCalendar(change: CalendarPatchDto): Promise<CalendarDto> {
		return this.#post<CalendarDto>('/api/calendars', change, 'PATCH');
	}

	exportLabels(): Promise<LabelExportDto> {
		return this.#json<LabelExportDto>('/api/labels/export');
	}

	importLabels(labels: LabelExportDto): Promise<LabelImportResultDto> {
		return this.#post<LabelImportResultDto>('/api/labels/import', labels);
	}

	/** Multipart upload (field `file`) with progress; `fetch` cannot report upload progress. */
	uploadAttachment(
		file: File,
		onProgress: (loaded: number, total: number) => void,
		signal?: AbortSignal
	): Promise<AttachmentDto> {
		return new Promise((resolve, reject) => {
			const xhr = new XMLHttpRequest();
			xhr.open('POST', '/api/attachments');
			xhr.setRequestHeader(CSRF_HEADER, CSRF_VALUE);
			xhr.responseType = 'json';
			xhr.upload.onprogress = (e) => onProgress(e.loaded, e.lengthComputable ? e.total : file.size);
			xhr.onload = () => {
				const body = xhr.response as AttachmentDto | ApiErrorDto | null;
				if (xhr.status >= 200 && xhr.status < 300 && body) return resolve(body as AttachmentDto);
				const error =
					body && 'error' in body
						? body
						: { error: 'http_' + xhr.status, message: xhr.statusText || `HTTP ${xhr.status}` };
				const err = new ApiError(xhr.status, error);
				if (xhr.status === 401) this.onUnauthorized(err);
				reject(err);
			};
			xhr.onerror = () => reject(new TypeError('network error'));
			xhr.onabort = () => reject(new DOMException('aborted', 'AbortError'));
			signal?.addEventListener('abort', () => xhr.abort(), { once: true });
			const form = new FormData();
			form.append('file', file, file.name);
			xhr.send(form);
		});
	}

	removeAttachment(id: string): Promise<void> {
		return this.#empty(`/api/attachments/${encodeURIComponent(id)}`, 'DELETE');
	}

	/** `keepalive` lets the save finish while the page unloads. */
	createDraft(draft: DraftInputDto, keepalive = false): Promise<DraftDto> {
		return this.#post<DraftDto>('/api/drafts', draft, 'POST', keepalive);
	}

	updateDraft(id: number, draft: DraftInputDto, keepalive = false): Promise<DraftDto> {
		return this.#post<DraftDto>(`/api/drafts/${id}`, draft, 'PUT', keepalive);
	}

	draft(id: number): Promise<DraftDto> {
		return this.#json<DraftDto>(`/api/drafts/${id}`);
	}

	deleteDraft(id: number): Promise<void> {
		return this.#empty(`/api/drafts/${id}`, 'DELETE');
	}

	send(body: SendDto): Promise<SendStatusDto> {
		return this.#post<SendStatusDto>('/api/send', body);
	}

	async outbox(): Promise<OutboxItemDto[]> {
		return (await this.#json<OutboxDto>('/api/outbox')).items;
	}

	cancelOutbox(id: number): Promise<OutboxItemDto> {
		return this.#post<OutboxItemDto>(`/api/outbox/${id}`, undefined, 'DELETE');
	}

	retryOutbox(id: number): Promise<OutboxItemDto> {
		return this.#post<OutboxItemDto>(`/api/outbox/${id}/retry`);
	}

	replyDraft(messageId: number, mode: ReplyMode): Promise<ReplyDraftDto> {
		return this.#json<ReplyDraftDto>(`/api/messages/${messageId}/reply?mode=${mode}`);
	}
}

export function renderUrl(messageId: number, images: boolean): string {
	return `/api/messages/${messageId}/render?images=${images ? 1 : 0}`;
}

export function downloadUrl(partUrl: string): string {
	return partUrl + (partUrl.includes('?') ? '&' : '?') + 'download=1';
}
