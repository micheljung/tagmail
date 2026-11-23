import type { DesktopStatusDto } from '@tagmail/api-types';
import { describe, expect, it, vi } from 'vitest';
import type { DraftInit } from '../compose/draft.svelte';
import { Api, ApiError } from '../transport/api';
import { HttpTransport } from '../transport/http';
import type { Listen } from '../transport/tauri';
import { DesktopStore, OFFLINE_TEXT, mailtoDraft } from './desktop.svelte';

const live: DesktopStatusDto = {
	paired: true,
	server: 'https://mail.example.com',
	online: true,
	phase: { kind: 'live' },
	lastSync: 1_795_000_000,
	warnings: []
};

function json(status: number, body: unknown): Response {
	return new Response(JSON.stringify(body), {
		status,
		headers: { 'content-type': 'application/json' }
	});
}

async function setup(routes: Record<string, () => Response>) {
	const fetch = vi.fn<typeof globalThis.fetch>((input) => {
		const route = routes[input as string];
		return Promise.resolve(route ? route() : json(404, { error: 'not_found', message: 'no' }));
	});
	const api = new Api(new HttpTransport({ fetch }));
	const handlers = new Map<string, (payload: unknown) => void>();
	const listen: Listen = (event, handler) => {
		handlers.set(event, handler as (payload: unknown) => void);
		return Promise.resolve(() => handlers.delete(event));
	};
	const drafts: DraftInit[] = [];
	const store = new DesktopStore();
	const stop = await store.start(api, listen, {
		mailto: (d) => drafts.push(d),
		pair: () => undefined
	});
	const emit = (event: string, payload: unknown) => handlers.get(event)?.(payload);
	return { api, store, emit, drafts, stop, handlers };
}

describe('DesktopStore', () => {
	it('shows an offline error verbatim until the server answers again', async () => {
		const message = 'Older message needs a connection';
		const { api, store, emit } = await setup({
			'/api/desktop/status': () => json(200, live),
			'/api/threads/9': () => json(503, { error: 'offline', message })
		});
		expect(store.offlineText).toBeNull();
		await expect(api.thread(9)).rejects.toBeInstanceOf(ApiError);
		expect(store.offlineText).toBe(message);
		emit('desktop:status', { ...live, online: false });
		expect(store.offlineText).toBe(message);
		emit('desktop:status', live);
		expect(store.offlineText).toBeNull();
		emit('desktop:status', { ...live, online: false });
		expect(store.offlineText).toBe(OFFLINE_TEXT);
	});

	it('tracks bootstrap progress and update-required from status and events', async () => {
		const { store, emit } = await setup({
			'/api/desktop/status': () =>
				json(200, { ...live, phase: { kind: 'bootstrapping', done: 250, total: 1000 } })
		});
		expect(store.progress).toBe(25);
		const v = (minor: number) => ({ major: 1, minor });
		emit('desktop:update-required', { server: v(3), minClient: v(2) });
		expect(store.updateRequired).toEqual({ server: v(3), minClient: v(2) });
		emit('desktop:status', live);
		expect(store.progress).toBeNull();
	});

	it('opens a prefilled draft for a mailto link and unlistens on stop', async () => {
		const { emit, drafts, stop, handlers } = await setup({});
		emit('desktop:mailto', {
			to: ['a@x.example', 'b@x.example'],
			cc: ['c@x.example'],
			bcc: [],
			subject: 'Hi',
			body: 'line 1\r\n<b>line 2</b>'
		});
		expect(drafts).toEqual([
			{
				to: [
					{ name: null, email: 'a@x.example' },
					{ name: null, email: 'b@x.example' }
				],
				cc: [{ name: null, email: 'c@x.example' }],
				bcc: [],
				subject: 'Hi',
				html: 'line 1<br>&lt;b&gt;line 2&lt;/b&gt;'
			}
		]);
		stop();
		expect(handlers.size).toBe(0);
	});
});

describe('mailtoDraft', () => {
	it('leaves missing fields empty', () => {
		expect(mailtoDraft({ to: [], cc: [], bcc: [], subject: null, body: null })).toEqual({
			to: [],
			cc: [],
			bcc: [],
			subject: '',
			html: ''
		});
	});
});
