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

	if (registration.waiting && container.controller) offer(registration.waiting);
	registration.addEventListener('updatefound', () => {
		const worker = registration.installing;
		worker?.addEventListener('statechange', () => {
			if (worker.state === 'installed' && container.controller) offer(worker);
		});
	});
	return registration;
}
