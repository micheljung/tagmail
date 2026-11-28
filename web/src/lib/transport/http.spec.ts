import type { ApiEvent } from '@tagmail/api-types';
import { describe, expect, it, vi } from 'vitest';
import { CSRF_HEADER, HttpTransport, type EventSourceLike } from './http';

class FakeEventSource implements EventSourceLike {
	readyState = 1;
	closed = false;
	readonly listeners = new Map<string, ((e: MessageEvent<string>) => void)[]>();
	constructor(readonly url: string) {}
	addEventListener(type: string, listener: (e: MessageEvent<string>) => void): void {
		this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
	}
	close(): void {
		this.closed = true;
		this.readyState = 2;
	}
	emit(type: string, data = '', lastEventId = ''): void {
		for (const l of this.listeners.get(type) ?? []) {
			l({ data, lastEventId } as MessageEvent<string>);
		}
	}
}

function setup() {
	const sources: FakeEventSource[] = [];
	const fetch = vi.fn<typeof globalThis.fetch>(() => Promise.resolve(new Response('{}')));
	const transport = new HttpTransport({
		fetch,
		eventSource: (url) => {
			const s = new FakeEventSource(url);
			sources.push(s);
			return s;
		}
	});
	return { transport, fetch, sources };
}

describe('HttpTransport.request', () => {
	it('adds the CSRF header to mutating requests only', async () => {
		const { transport, fetch } = setup();
		await transport.request('/api/threads?ids=1');
		await transport.request('/api/auth/login', { method: 'post', body: '{}' });
		const [get, post] = fetch.mock.calls.map((c) => new Headers(c[1]?.headers));
		expect(get.has(CSRF_HEADER)).toBe(false);
		expect(post.get(CSRF_HEADER)).toBe('tagmail');
		expect(fetch.mock.calls[1][1]?.credentials).toBe('same-origin');
	});

	it('reports whether the server answered', async () => {
		const reports: boolean[] = [];
		const failing = vi.fn<typeof globalThis.fetch>(() =>
			Promise.reject(new TypeError('Failed to fetch'))
		);
		const down = new HttpTransport({ fetch: failing, onReachable: (r) => reports.push(r) });
		await expect(down.request('/api/labels')).rejects.toThrow(TypeError);
		const aborting = vi.fn<typeof globalThis.fetch>(() =>
			Promise.reject(new DOMException('aborted', 'AbortError'))
		);
		const aborted = new HttpTransport({ fetch: aborting, onReachable: (r) => reports.push(r) });
		await expect(aborted.request('/api/labels')).rejects.toThrow('aborted');
		const answering = vi.fn<typeof globalThis.fetch>(() =>
			Promise.resolve(new Response('', { status: 502 }))
		);
		const up = new HttpTransport({ fetch: answering, onReachable: (r) => reports.push(r) });
		await up.request('/api/labels');
		expect(reports).toEqual([false, true]);
	});
});

describe('HttpTransport.events', () => {
	it('starts after the given cursor and forwards changes', () => {
		const { transport, sources } = setup();
		const events: ApiEvent[] = [];
		transport.events('00ab:7', (e) => events.push(e));
		expect(sources[0].url).toBe('/api/events?since=00ab%3A7&include=calendar');
		const page = { changes: [], next: '00ab:9', more: false };
		sources[0].emit('changes', JSON.stringify(page), '00ab:9');
		expect(events).toEqual([{ type: 'changes', id: '00ab:9', data: page }]);
	});

	it('closes the stream on reset', () => {
		const { transport, sources } = setup();
		const events: ApiEvent[] = [];
		transport.events(undefined, (e) => events.push(e));
		expect(sources[0].url).toBe('/api/events?include=calendar');
		sources[0].emit('reset', JSON.stringify({ error: 'gone', message: 'reload' }));
		expect(sources[0].closed).toBe(true);
		expect(events).toEqual([{ type: 'reset', data: { error: 'gone', message: 'reload' } }]);
	});

	it('reports a stream the browser gave up on as a reset, but not a retrying one', () => {
		const { transport, sources } = setup();
		const events: ApiEvent[] = [];
		transport.events(undefined, (e) => events.push(e));
		sources[0].readyState = 0;
		sources[0].emit('error');
		expect(events).toEqual([]);
		sources[0].readyState = 2;
		sources[0].emit('error');
		expect(events.map((e) => e.type)).toEqual(['reset']);
	});

	it('stops delivering after unsubscribe', () => {
		const { transport, sources } = setup();
		const events: ApiEvent[] = [];
		const stop = transport.events(undefined, (e) => events.push(e));
		stop();
		sources[0].emit('changes', JSON.stringify({ changes: [], next: 'a:1', more: false }));
		expect(sources[0].closed).toBe(true);
		expect(events).toEqual([]);
	});
});
