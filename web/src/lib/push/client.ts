import type {
	PushKeyDto,
	PushSubscribeDto,
	PushSubscriptionDto,
	PushSubscriptionPatchDto
} from '@tagmail/api-types';

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

function subscribeDto(
	subscription: PushSubscription,
	settings: Pick<PushSubscribeDto, 'preview' | 'accountIds'> = {}
): PushSubscribeDto {
	const json = subscription.toJSON();
	return {
		endpoint: subscription.endpoint,
		keys: { p256dh: json.keys?.p256dh ?? '', auth: json.keys?.auth ?? '' },
		...settings
	};
}

function settingsOf(row: PushSubscriptionDto | undefined) {
	return row ? { preview: row.preview, accountIds: row.accountIds ?? undefined } : {};
}

/**
 * App start: registers this browser's subscription again, so a new session
 * keeps receiving pushes, and renews it when the server key changed.
 */
export async function syncPush(
	manager: PushManager,
	api: PushApi
): Promise<PushSubscriptionDto | null> {
	const existing = await manager.getSubscription();
	if (!existing) return null;
	const { publicKey } = await api.pushKey();
	if (sameKey(existing, publicKey)) return api.subscribePush(subscribeDto(existing));
	const previous = (await api.pushSubscriptions()).find((s) => s.current);
	await existing.unsubscribe();
	const renewed = await manager.subscribe({
		userVisibleOnly: true,
		applicationServerKey: publicKey
	});
	return api.subscribePush(subscribeDto(renewed, settingsOf(previous)));
}

/**
 * Asks for permission and subscribes. Call it straight from a click: iOS
 * only shows the prompt during a user gesture.
 */
export async function enablePush(
	manager: PushManager,
	api: PushApi,
	requestPermission: () => Promise<NotificationPermission>
): Promise<PushSubscriptionDto | null> {
	if ((await requestPermission()) !== 'granted') return null;
	const { publicKey } = await api.pushKey();
	const existing = await manager.getSubscription();
	if (existing && sameKey(existing, publicKey)) return api.subscribePush(subscribeDto(existing));
	await existing?.unsubscribe();
	const created = await manager.subscribe({
		userVisibleOnly: true,
		applicationServerKey: publicKey
	});
	return api.subscribePush(subscribeDto(created));
}

export async function disablePush(
	manager: PushManager,
	api: PushApi,
	row: PushSubscriptionDto | null
): Promise<void> {
	await (await manager.getSubscription())?.unsubscribe();
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
