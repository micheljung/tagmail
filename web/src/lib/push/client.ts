import type {
	AccountsDto,
	AccountDto,
	PushKeyDto,
	PushSubscribeDto,
	PushSubscriptionDto,
	PushSubscriptionPatchDto
} from '@tagmail/api-types';
import { CSRF_HEADER, CSRF_VALUE } from '../transport/http';
import { defaultReminders, type PushPrefStore, type PushPrefs } from './prefs';

export type PushAvailability = 'unsupported' | 'ios-tab' | 'blocked' | 'ready';

export interface PushEnv {
	userAgent: string;
	/** `navigator.standalone`: `false` in an iOS Safari tab, absent elsewhere. */
	standalone: boolean | undefined;
	serviceWorker: boolean;
	pushManager: boolean;
	notification: boolean;
	permission: NotificationPermission | undefined;
}

export function pushEnv(): PushEnv {
	const nav = navigator as Navigator & { standalone?: boolean };
	const notification = 'Notification' in globalThis;
	return {
		userAgent: nav.userAgent,
		standalone: nav.standalone,
		serviceWorker: 'serviceWorker' in nav,
		pushManager: 'PushManager' in globalThis,
		notification,
		permission: notification ? Notification.permission : undefined
	};
}

/** iOS only offers Web Push to apps added to the Home Screen. */
export function pushAvailability(env: PushEnv): PushAvailability {
	const iosTab = /iPad|iPhone/.test(env.userAgent) || env.standalone === false;
	if (iosTab && !env.pushManager) return 'ios-tab';
	if (!env.serviceWorker || !env.pushManager || !env.notification) return 'unsupported';
	return env.permission === 'denied' ? 'blocked' : 'ready';
}

export interface PushApi {
	accounts(): Promise<AccountDto[]>;
	pushKey(): Promise<PushKeyDto>;
	pushSubscriptions(): Promise<PushSubscriptionDto[]>;
	subscribePush(subscription: PushSubscribeDto): Promise<PushSubscriptionDto>;
	updatePushSubscription(id: number, patch: PushSubscriptionPatchDto): Promise<PushSubscriptionDto>;
	removePushSubscription(id: number): Promise<void>;
}

export function base64url(bytes: ArrayBuffer | Uint8Array): string {
	const view = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
	let binary = '';
	for (const byte of view) binary += String.fromCharCode(byte);
	return btoa(binary).replaceAll('+', '-').replaceAll('/', '_').replace(/=+$/, '');
}

function sameKey(subscription: PushSubscription, publicKey: string): boolean {
	const key = subscription.options.applicationServerKey;
	return key !== null && base64url(key) === publicKey;
}

type Settings = Pick<PushSubscribeDto, 'preview' | 'accountIds' | 'reminders'>;
type RegisterApi = Pick<PushApi, 'accounts' | 'subscribePush'>;

const REJECTED_STATUSES = [400, 422];

/** The server refused the account filter, e.g. an older server naming a removed account. */
function rejectsAccounts(error: unknown): boolean {
	if (typeof error !== 'object' || error === null) return false;
	const { status, field } = error as { status?: unknown; field?: unknown };
	return typeof status === 'number' && REJECTED_STATUSES.includes(status) && field === 'accountIds';
}

/**
 * The saved settings, without accounts removed since they were saved; none
 * left means all accounts. If the accounts can't be listed the saved filter
 * is sent as is and a refusal is handled by `register`.
 */
async function settingsOf(api: RegisterApi, prefs: PushPrefs | undefined): Promise<Settings> {
	if (!prefs) return {};
	const base = {
		preview: prefs.preview,
		...(prefs.reminders === undefined ? {} : { reminders: prefs.reminders })
	};
	if (prefs.accountIds === null) return base;
	const known = await api.accounts().then(
		(accounts) => new Set(accounts.map((a) => a.id)),
		() => null
	);
	const accountIds = known ? prefs.accountIds.filter((id) => known.has(id)) : prefs.accountIds;
	return accountIds.length > 0 ? { ...base, accountIds } : base;
}

/** A change that hangs longer than this gives way to the ones behind it. */
export const PUSH_OP_TIMEOUT_MS = 30_000;

let queue: Promise<unknown> = Promise.resolve();

/**
 * Runs settings changes one at a time in the order asked for, so a slow
 * re-registration can't overwrite a toggle the user made meanwhile.
 */
function serialized<T>(op: () => Promise<T>): Promise<T> {
	const run = queue.then(() => {
		let timer: ReturnType<typeof setTimeout> | undefined;
		const timeout = new Promise<never>(
			(_, reject) =>
				(timer = setTimeout(
					() => reject(new Error('Push settings change timed out')),
					PUSH_OP_TIMEOUT_MS
				))
		);
		return Promise.race([op(), timeout]).finally(() => clearTimeout(timer));
	});
	queue = run.catch(() => {});
	return run;
}

/**
 * Posts `subscription` with this device's saved settings and remembers what
 * the server applied. A changed endpoint names its predecessor, so the
 * server replaces that row instead of keeping a dead one.
 */
async function register(
	api: RegisterApi,
	prefs: PushPrefStore,
	subscription: PushSubscription,
	saved: PushPrefs | undefined,
	previousEndpoint = saved?.endpoint,
	reminders?: boolean
): Promise<PushSubscriptionDto> {
	const json = subscription.toJSON();
	const { endpoint } = subscription;
	const { accountIds, ...settings } = await settingsOf(api, saved);
	if (settings.reminders === undefined && reminders !== undefined) settings.reminders = reminders;
	const dto: PushSubscribeDto = {
		endpoint,
		keys: { p256dh: json.keys?.p256dh ?? '', auth: json.keys?.auth ?? '' },
		...settings,
		...(previousEndpoint && previousEndpoint !== endpoint ? { oldEndpoint: previousEndpoint } : {})
	};
	const row = await (accountIds
		? api.subscribePush({ ...dto, accountIds }).catch((error: unknown) => {
				if (!rejectsAccounts(error)) throw error;
				return api.subscribePush(dto);
			})
		: api.subscribePush(dto));
	await prefs.set({
		endpoint,
		preview: row.preview,
		accountIds: row.accountIds,
		...(settings.reminders === undefined ? {} : { reminders: row.reminders })
	});
	return row;
}

/**
 * App start: registers this browser's subscription again, so a new session
 * keeps receiving pushes, and renews it when the server key changed.
 * Settings saved before reminders existed get `reminders` once: the server
 * keeps them off for such a subscription.
 */
export async function syncPush(
	manager: PushManager,
	api: PushApi,
	prefs: PushPrefStore,
	reminders = defaultReminders()
): Promise<PushSubscriptionDto | null> {
	return serialized(async () => {
		const existing = await manager.getSubscription();
		if (!existing) return null;
		const saved = await prefs.get();
		const { publicKey } = await api.pushKey();
		let row: PushSubscriptionDto;
		if (sameKey(existing, publicKey)) row = await register(api, prefs, existing, saved);
		else {
			await existing.unsubscribe();
			const renewed = await manager.subscribe({
				userVisibleOnly: true,
				applicationServerKey: publicKey
			});
			row = await register(api, prefs, renewed, saved, existing.endpoint);
		}
		if (!saved || saved.reminders !== undefined) return row;
		row = await api.updatePushSubscription(row.id, { reminders });
		const stored = await prefs.get();
		if (stored) await prefs.set({ ...stored, reminders: row.reminders });
		return row;
	});
}

/**
 * Runs `sync` once per session: concurrent calls for the same session share
 * one run, and a failed run is retried on the next call.
 */
export function pushSyncer(sync: () => Promise<unknown>): (sessionId: string) => Promise<void> {
	let synced: string | null = null;
	let running: { id: string; done: Promise<void> } | null = null;
	return (id) => {
		if (id === synced) return Promise.resolve();
		if (running?.id === id) return running.done;
		const done = sync().then(
			() => {
				synced = id;
			},
			(error: unknown) => console.warn('push subscription sync failed', error)
		);
		const run = {
			id,
			done: done.finally(() => {
				if (running === run) running = null;
			})
		};
		running = run;
		return run.done;
	};
}

/**
 * Asks for permission and subscribes. Call it straight from a click: iOS
 * only shows the prompt during a user gesture.
 */
export async function enablePush(
	manager: PushManager,
	api: PushApi,
	prefs: PushPrefStore,
	requestPermission: () => Promise<NotificationPermission>,
	reminders = defaultReminders()
): Promise<PushSubscriptionDto | null> {
	if ((await requestPermission()) !== 'granted') return null;
	return serialized(async () => {
		const saved = await prefs.get();
		const { publicKey } = await api.pushKey();
		const existing = await manager.getSubscription();
		if (existing && sameKey(existing, publicKey))
			return register(api, prefs, existing, saved, saved?.endpoint, reminders);
		await existing?.unsubscribe();
		const created = await manager.subscribe({
			userVisibleOnly: true,
			applicationServerKey: publicKey
		});
		return register(api, prefs, created, saved, existing?.endpoint ?? saved?.endpoint, reminders);
	});
}

/** Changes this device's settings and remembers them for re-registration. */
export async function updatePush(
	api: PushApi,
	prefs: PushPrefStore,
	id: number,
	patch: PushSubscriptionPatchDto
): Promise<PushSubscriptionDto> {
	return serialized(async () => {
		const row = await api.updatePushSubscription(id, patch);
		const saved = await prefs.get();
		if (saved)
			await prefs.set({
				...saved,
				preview: row.preview,
				accountIds: row.accountIds,
				reminders: row.reminders
			});
		return row;
	});
}

/**
 * `pushsubscriptionchange`: subscribes again and registers the new endpoint
 * in place of the old one. Chrome passes no old subscription, so the key
 * then comes from the server and the old endpoint from the saved settings.
 */
export async function renewPush(
	manager: PushManager,
	api: Pick<PushApi, 'accounts' | 'pushKey' | 'subscribePush'>,
	prefs: PushPrefStore,
	old: PushSubscription | null
): Promise<PushSubscriptionDto> {
	return serialized(async () => {
		const saved = await prefs.get();
		const key = old?.options.applicationServerKey;
		const applicationServerKey = key ? base64url(key) : (await api.pushKey()).publicKey;
		const renewed = await manager.subscribe({ userVisibleOnly: true, applicationServerKey });
		return register(api, prefs, renewed, saved, old?.endpoint ?? saved?.endpoint);
	});
}

/** The push API over plain `fetch`, for the service worker. */
export function fetchPushApi(
	fetcher: typeof fetch = fetch
): Pick<PushApi, 'accounts' | 'pushKey' | 'subscribePush'> {
	const json = async <T>(path: string, init?: RequestInit): Promise<T> => {
		const response = await fetcher(path, { credentials: 'same-origin', ...init });
		if (!response.ok) {
			const body = (await response.json().catch(() => ({}))) as { field?: string };
			throw Object.assign(new Error(`${path} answered ${response.status}`), {
				status: response.status,
				field: body.field
			});
		}
		return (await response.json()) as T;
	};
	return {
		accounts: async () => (await json<AccountsDto>('/api/accounts')).accounts,
		pushKey: () => json<PushKeyDto>('/api/push/key'),
		subscribePush: (subscription) =>
			json<PushSubscriptionDto>('/api/push/subscriptions', {
				method: 'POST',
				headers: { 'content-type': 'application/json', [CSRF_HEADER]: CSRF_VALUE },
				body: JSON.stringify(subscription)
			})
	};
}

/** Dismisses a reminder on every device. */
export async function dismissReminder(
	alarmKey: string,
	fetcher: typeof fetch = fetch
): Promise<void> {
	await fetcher(`/api/reminders/${encodeURIComponent(alarmKey)}/dismiss`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { [CSRF_HEADER]: CSRF_VALUE }
	});
}

/** Closes reminders dismissed elsewhere. */
export async function closeReminders(
	alarmKeys: string[],
	registration: Promise<ServiceWorkerRegistration | null>
): Promise<void> {
	const shown = await registration;
	if (!shown) return;
	for (const tag of alarmKeys)
		for (const notification of await shown.getNotifications({ tag })) notification.close();
}

export async function disablePush(
	manager: PushManager,
	api: PushApi,
	prefs: PushPrefStore,
	row: PushSubscriptionDto | null
): Promise<void> {
	return serialized(async () => {
		await (await manager.getSubscription())?.unsubscribe();
		await prefs.clear();
		if (row) await api.removePushSubscription(row.id);
	});
}

export const LOGOUT_UNSUBSCRIBE_MS = 2000;

/** Best effort before logout; the server drops the session's subscriptions anyway. */
export async function unsubscribeQuietly(
	manager: () => Promise<PushManager | null>,
	timeoutMs = LOGOUT_UNSUBSCRIBE_MS
): Promise<void> {
	const unsubscribe = (async () => {
		await (await (await manager())?.getSubscription())?.unsubscribe();
	})().catch(() => {});
	let timer: ReturnType<typeof setTimeout> | undefined;
	const timeout = new Promise<void>((resolve) => (timer = setTimeout(resolve, timeoutMs)));
	await Promise.race([unsubscribe, timeout]);
	clearTimeout(timer);
}
