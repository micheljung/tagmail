import type { ApiEvent, ChangesDto } from '@tagmail/api-types';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { HttpTransport } from './http';
import { createTransport } from './index';
import { TauriTransport, isDesktop, type Listen } from './tauri';

function setup(status = 204) {
	const handlers = new Map<string, (payload: unknown) => void>();
	const unlistened: string[] = [];
	const listen: Listen = (event, handler) => {
		handlers.set(event, handler as (payload: unknown) => void);
		return Promise.resolve(() => {
			handlers.delete(event);
			unlistened.push(event);
		});
	};
	const fetch = vi.fn<typeof globalThis.fetch>(() =>
		Promise.resolve(new Response(null, { status }))
	);
	const transport = new TauriTransport({ fetch, listen });
	const emit = (event: string, payload: unknown) => handlers.get(event)?.(payload);
	return { transport, fetch, emit, handlers, unlistened };
}

const flush = () => new Promise((r) => setTimeout(r, 0));

const page: ChangesDto = {
	changes: [{ seq: 4, entity: 'thread', id: 7, op: { kind: 'upsert' }, payload: null }],
	next: '000000000000000a:4',
	more: false
};

describe('transport selection', () => {
	afterEach(() => {
		delete (globalThis as Record<string, unknown>).__TAURI_INTERNALS__;
	});

	it('uses HTTP in the browser and Tauri inside the desktop app', () => {
		expect(isDesktop()).toBe(false);
		expect(createTransport()).toBeInstanceOf(HttpTransport);
		(globalThis as Record<string, unknown>).__TAURI_INTERNALS__ = {};
		expect(isDesktop()).toBe(true);
		expect(createTransport()).toBeInstanceOf(TauriTransport);
	});
});

describe('TauriTransport.events', () => {
	it('starts the local stream after listening and maps changes and resets', async () => {
		const { transport, fetch, emit, unlistened } = setup();
		const events: ApiEvent[] = [];
		transport.events('000000000000000a:3', (e) => events.push(e));
		await flush();
		expect(fetch.mock.calls[0][0]).toBe('/api/desktop/events/start?since=000000000000000a%3A3');
		emit('api:changes', page);
		expect(events).toEqual([{ type: 'changes', id: page.next, data: page }]);
		emit('api:reset', { error: 'gone', message: 'reload' });
		expect(events[1]).toEqual({ type: 'reset', data: { error: 'gone', message: 'reload' } });
		expect(unlistened.sort()).toEqual(['api:changes', 'api:reset']);
	});

	it('stops delivering after unsubscribe', async () => {
		const { transport, emit, handlers } = setup();
		const events: ApiEvent[] = [];
		const stop = transport.events(undefined, (e) => events.push(e));
		await flush();
		stop();
		emit('api:changes', page);
		expect(events).toEqual([]);
		expect(handlers.size).toBe(0);
	});

	it('unlistens when unsubscribed before the listeners were registered', async () => {
		const { transport, fetch, handlers } = setup();
		const stop = transport.events(undefined, () => undefined);
		stop();
		await flush();
		expect(handlers.size).toBe(0);
		expect(fetch).not.toHaveBeenCalled();
	});

	it('reports a reset when the stream cannot start', async () => {
		const { transport } = setup(500);
		const events: ApiEvent[] = [];
		transport.events(undefined, (e) => events.push(e));
		await flush();
		expect(events).toEqual([
			{
				type: 'reset',
				data: { error: 'gone', message: 'the event stream closed; reload the views' }
			}
		]);
	});
});
