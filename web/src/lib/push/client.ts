import type {
	PushKeyDto,
	PushSubscribeDto,
	PushSubscriptionDto,
	PushSubscriptionPatchDto
} from '@tagmail/api-types';
import { CSRF_HEADER, CSRF_VALUE } from '../transport/http';
import type { PushPrefStore, PushPrefs } from './prefs';

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

type Settings = Pick<PushSubscribeDto, 'preview' | 'accountIds'>;

function settingsOf(prefs: PushPrefs | undefined): Settings {
	if (!prefs) return {};
	return prefs.accountIds === null
		? { preview: prefs.preview }
		: { preview: prefs.preview, accountIds: prefs.accountIds };
}

/**
 * Posts `subscription` with this device's saved settings and remembers what
 * the server applied. A changed endpoint names its predecessor, so the
 * server replaces that row instead of keeping a dead one.
 */
async function register(
	api: Pick<PushApi, 'subscribePush'>,
	prefs: PushPrefStore,
	subscription: PushSubscription,
	saved: PushPrefs | undefined,
	previousEndpoint = saved?.endpoint
): Promise<PushSubscriptionDto> {
	const json = subscription.toJSON();
	const { endpoint } = subscription;
	const row = await api.subscribePush({
		endpoint,
		keys: { p256dh: json.keys?.p256dh ?? '', auth: json.keys?.auth ?? '' },
		...settingsOf(saved),
		...(previousEndpoint && previousEndpoint !== endpoint ? { oldEndpoint: previousEndpoint } : {})
	});
	await prefs.set({ endpoint, preview: row.preview, accountIds: row.accountIds });
	return row;
}

/**
 * App start: registers this browser's subscription again, so a new session
 * keeps receiving pushes, and renews it when the server key changed.
 */
export async function syncPush(
	manager: PushManager,
	api: PushApi,
	prefs: PushPrefStore
): Promise<PushSubscriptionDto | null> {
	const existing = await manager.getSubscription();
	if (!existing) return null;
	const saved = await prefs.get();
	const { publicKey } = await api.pushKey();
	if (sameKey(existing, publicKey)) return register(api, prefs, existing, saved);
	await existing.unsubscribe();
	const renewed = await manager.subscribe({
		userVisibleOnly: true,
		applicationServerKey: publicKey
	});
	return register(api, prefs, renewed, saved, existing.endpoint);
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
	requestPermission: () => Promise<NotificationPermission>
): Promise<PushSubscriptionDto | null> {
	if ((await requestPermission()) !== 'granted') return null;
	const saved = await prefs.get();
	const { publicKey } = await api.pushKey();
	const existing = await manager.getSubscription();
	if (existing && sameKey(existing, publicKey)) return register(api, prefs, existing, saved);
	await existing?.unsubscribe();
	const created = await manager.subscribe({
		userVisibleOnly: true,
		applicationServerKey: publicKey
	});
	return register(api, prefs, created, saved, existing?.endpoint ?? saved?.endpoint);
}

/** Changes this device's settings and remembers them for re-registration. */
export async function updatePush(
	api: PushApi,
	prefs: PushPrefStore,
	id: number,
	patch: PushSubscriptionPatchDto
): Promise<PushSubscriptionDto> {
	const row = await api.updatePushSubscription(id, patch);
	const saved = await prefs.get();
	if (saved) await prefs.set({ ...saved, preview: row.preview, accountIds: row.accountIds });
	return row;
}

/**
 * `pushsubscriptionchange`: subscribes again and registers the new endpoint
 * in place of the old one. Chrome passes no old subscription, so the key
 * then comes from the server and the old endpoint from the saved settings.
 */
export async function renewPush(
	manager: PushManager,
	api: Pick<PushApi, 'pushKey' | 'subscribePush'>,
	prefs: PushPrefStore,
	old: PushSubscription | null
): Promise<PushSubscriptionDto> {
	const saved = await prefs.get();
	const key = old?.options.applicationServerKey;
	const applicationServerKey = key ? base64url(key) : (await api.pushKey()).publicKey;
	const renewed = await manager.subscribe({ userVisibleOnly: true, applicationServerKey });
	return register(api, prefs, renewed, saved, old?.endpoint ?? saved?.endpoint);
}

/** The push API over plain `fetch`, for the service worker. */
export function fetchPushApi(
	fetcher: typeof fetch = fetch
): Pick<PushApi, 'pushKey' | 'subscribePush'> {
	const json = async <T>(path: string, init?: RequestInit): Promise<T> => {
		const response = await fetcher(path, { credentials: 'same-origin', ...init });
		if (!response.ok) throw new Error(`${path} answered ${response.status}`);
		return (await response.json()) as T;
	};
	return {
		pushKey: () => json<PushKeyDto>('/api/push/key'),
		subscribePush: (subscription) =>
			json<PushSubscriptionDto>('/api/push/subscriptions', {
				method: 'POST',
				headers: { 'content-type': 'application/json', [CSRF_HEADER]: CSRF_VALUE },
				body: JSON.stringify(subscription)
			})
	};
}

export async function disablePush(
	manager: PushManager,
	api: PushApi,
	prefs: PushPrefStore,
	row: PushSubscriptionDto | null
): Promise<void> {
	await (await manager.getSubscription())?.unsubscribe();
	await prefs.clear();
	if (row) await api.removePushSubscription(row.id);
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
