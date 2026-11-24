import { safeAppUrl } from './sw';

export const SERVICE_WORKER_URL = '/service-worker.js';

export interface RegisterEnv {
	dev: boolean;
	desktop: boolean;
	secure: boolean;
	supported: boolean;
}

/** The desktop app's `app://` origin has no service workers; the replica covers offline there. */
export function shouldRegister(env: RegisterEnv): boolean {
	return !env.dev && !env.desktop && env.secure && env.supported;
}

let share: (registration: Promise<ServiceWorkerRegistration> | null) => void = () => {};

/**
 * The registration the root layout makes, or `null` where it makes none.
 * Pages mount before the root layout does, so they wait on this.
 */
export const appRegistration = new Promise<ServiceWorkerRegistration | null>((resolve) => {
	share = (registration) => resolve(registration?.catch(() => null) ?? null);
});

export function shareRegistration(registration: Promise<ServiceWorkerRegistration> | null): void {
	share(registration);
}

export interface RegisterHooks {
	/** A new version is installed; `apply` activates it and reloads once. */
	onUpdate: (apply: () => void) => void;
	/** A notification asked an uncontrolled window to open an app path. */
	onNavigate: (path: string) => void;
}

/** Registers the service worker and wires the update and navigation messages. */
export async function registerServiceWorker(
	container: ServiceWorkerContainer,
	hooks: RegisterHooks,
	reload: () => void = () => location.reload()
): Promise<ServiceWorkerRegistration> {
	container.addEventListener('message', (event: MessageEvent<unknown>) => {
		const data = event.data as { type?: unknown; url?: unknown } | null;
		if (data?.type === 'navigate') hooks.onNavigate(safeAppUrl(data.url, location.origin));
	});

	const registration = await container.register(SERVICE_WORKER_URL, {
		scope: '/',
		type: 'module'
	});

	let reloading = false;
	const offer = (worker: ServiceWorker) =>
		hooks.onUpdate(() => {
			container.addEventListener('controllerchange', () => {
				if (reloading) return;
				reloading = true;
				reload();
			});
			worker.postMessage({ type: 'skipWaiting' });
		});

	// WebKit can deliver "installed" after the first worker has already
	// claimed the page, so whether this is an update is decided up front.
	const watched = new WeakSet<ServiceWorker>();
	const watch = (worker: ServiceWorker | null) => {
		if (!worker || container.controller === null || watched.has(worker)) return;
		watched.add(worker);
		worker.addEventListener('statechange', () => {
			if (worker.state === 'installed') offer(worker);
		});
	};

	if (registration.waiting && container.controller) offer(registration.waiting);
	watch(registration.installing);
	registration.addEventListener('updatefound', () => watch(registration.installing));
	return registration;
}
