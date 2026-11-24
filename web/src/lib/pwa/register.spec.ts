import { afterEach, describe, expect, it, vi } from 'vitest';
import {
	SERVICE_WORKER_URL,
	registerServiceWorker,
	shouldRegister,
	type RegisterEnv
} from './register';

const WEB: RegisterEnv = { dev: false, desktop: false, secure: true, supported: true };

describe('shouldRegister', () => {
	it.each([
		['the web app', WEB, true],
		['the desktop app', { ...WEB, desktop: true }, false],
		['the dev server', { ...WEB, dev: true }, false],
		['an insecure origin', { ...WEB, secure: false }, false],
		['a browser without service workers', { ...WEB, supported: false }, false]
	])('%s', (_name, env, expected) => {
		expect(shouldRegister(env)).toBe(expected);
	});
});

class FakeWorker extends EventTarget {
	state = 'installing';
	readonly postMessage = vi.fn();
	moveTo(state: string) {
		this.state = state;
		this.dispatchEvent(new Event('statechange'));
	}
}

class FakeRegistration extends EventTarget {
	installing: FakeWorker | null = null;
	waiting: FakeWorker | null = null;
	install(worker: FakeWorker) {
		this.installing = worker;
		this.dispatchEvent(new Event('updatefound'));
	}
}

class FakeContainer extends EventTarget {
	controller: object | null;
	readonly registration = new FakeRegistration();
	readonly register = vi.fn(() => Promise.resolve(this.registration));
	constructor(controlled: boolean) {
		super();
		this.controller = controlled ? {} : null;
	}
	send(data: unknown) {
		this.dispatchEvent(new MessageEvent('message', { data }));
	}
}

async function setup(controlled: boolean, waiting?: FakeWorker) {
	vi.stubGlobal('location', new URL('https://mail.example.com/inbox'));
	const container = new FakeContainer(controlled);
	container.registration.waiting = waiting ?? null;
	const updates: (() => void)[] = [];
	const paths: string[] = [];
	const reload = vi.fn();
	await registerServiceWorker(
		container as unknown as ServiceWorkerContainer,
		{ onUpdate: (apply) => updates.push(apply), onNavigate: (path) => paths.push(path) },
		reload
	);
	return { container, updates, paths, reload };
}

describe('registerServiceWorker', () => {
	afterEach(() => vi.unstubAllGlobals());

	it('registers the module worker at the root scope', async () => {
		const { container } = await setup(false);
		expect(container.register).toHaveBeenCalledWith(SERVICE_WORKER_URL, {
			scope: '/',
			type: 'module'
		});
	});

	it('offers no update for the first install', async () => {
		const { container, updates, reload } = await setup(false);
		const worker = new FakeWorker();
		container.registration.install(worker);
		worker.moveTo('installed');
		container.dispatchEvent(new Event('controllerchange'));
		expect(updates).toHaveLength(0);
		expect(reload).not.toHaveBeenCalled();
	});

	it('offers no update when the first install claims the page before "installed" is seen', async () => {
		const { container, updates } = await setup(false);
		const worker = new FakeWorker();
		container.registration.install(worker);
		container.controller = {};
		worker.moveTo('installed');
		expect(updates).toHaveLength(0);
	});

	it('offers an installed update and reloads once after it takes over', async () => {
		const { container, updates, reload } = await setup(true);
		const worker = new FakeWorker();
		container.registration.install(worker);
		worker.moveTo('installed');
		expect(updates).toHaveLength(1);
		container.dispatchEvent(new Event('controllerchange'));
		expect(reload).not.toHaveBeenCalled();

		updates[0]();
		expect(worker.postMessage).toHaveBeenCalledWith({ type: 'skipWaiting' });
		container.dispatchEvent(new Event('controllerchange'));
		container.dispatchEvent(new Event('controllerchange'));
		expect(reload).toHaveBeenCalledTimes(1);
	});

	it('offers a worker that was already waiting', async () => {
		const waiting = new FakeWorker();
		waiting.state = 'installed';
		const { updates } = await setup(true, waiting);
		updates[0]();
		expect(waiting.postMessage).toHaveBeenCalledWith({ type: 'skipWaiting' });
	});

	it('navigates only to same-origin app paths', async () => {
		const { container, paths } = await setup(true);
		container.send({ type: 'navigate', url: '/thread/4711' });
		container.send({ type: 'navigate', url: '//evil.test/x' });
		container.send({ type: 'navigate', url: 'javascript:alert(1)' });
		container.send({ type: 'other', url: '/thread/1' });
		container.send(null);
		expect(paths).toEqual(['/thread/4711', '/inbox', '/inbox']);
	});
});
