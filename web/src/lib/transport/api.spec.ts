import type { ApiTransport } from '@tagmail/api-types';
import { describe, expect, it, vi } from 'vitest';
import { encodeIdIndex } from '../../../e2e/mock/idIndex';
import { Api, ApiError, CURSOR_HEADER, PARTIAL_HEADER, downloadUrl, renderUrl } from './api';

function api(handler: (path: string, init?: RequestInit) => Response) {
	const transport: ApiTransport = {
		request: (path, init) => Promise.resolve(handler(path, init)),
		events: () => () => {}
	};
	return new Api(transport);
}

describe('Api', () => {
	it('runs the logout hook first and logs out even when it fails', async () => {
		const seen: string[] = [];
		const client = api((path) => {
			seen.push(path);
			return new Response(null, { status: 204 });
		});
		client.beforeLogout = () => {
			seen.push('hook');
			return Promise.reject(new Error('no service worker'));
		};
		await client.logout();
		expect(seen).toEqual(['hook', '/api/auth/logout']);
	});

	it('decodes a view index with its cursor', async () => {
		const seen: string[] = [];
		const client = api((path) => {
			seen.push(path);
			return new Response(encodeIdIndex([9, 4, 7]), { headers: { [CURSOR_HEADER]: 'e:3' } });
		});
		const index = await client.viewIndex('search:after:2026/01/01 "a b"');
		expect(Array.from(index.ids)).toEqual([9, 4, 7]);
		expect(index.cursor).toBe('e:3');
		expect(index.partial).toBe(false);
		expect(seen[0]).toBe('/api/views/search%3Aafter%3A2026%2F01%2F01%20%22a%20b%22/index');
	});

	it('flags a search the desktop answered from local mail only', async () => {
		const client = api(
			() =>
				new Response(encodeIdIndex([2]), {
					headers: { [CURSOR_HEADER]: 'e:3', [PARTIAL_HEADER]: 'offline' }
				})
		);
		expect((await client.viewIndex('search:old')).partial).toBe(true);
	});

	it('posts a view sync and treats 202 as done', async () => {
		const seen: [string, string | undefined][] = [];
		const client = api((path, init) => {
			seen.push([path, init?.method]);
			return path.includes('mailbox')
				? new Response(JSON.stringify({ error: 'not_found', message: 'no such mailbox' }), {
						status: 404
					})
				: new Response(null, { status: 202 });
		});
		await expect(client.syncView('account:2/label:a b')).resolves.toBeUndefined();
		expect(seen[0]).toEqual(['/api/views/account%3A2%2Flabel%3Aa%20b/sync', 'POST']);
		await expect(client.syncView('mailbox:9')).rejects.toMatchObject({
			status: 404,
			code: 'not_found'
		});
	});

	it('turns error bodies into ApiError and reports 401 outside auth routes', async () => {
		const onUnauthorized = vi.fn();
		const client = api(
			() =>
				new Response(JSON.stringify({ error: 'unauthorized', message: 'sign in' }), {
					status: 401
				})
		);
		client.onUnauthorized = onUnauthorized;
		await expect(client.threads([1, 2])).rejects.toMatchObject({
			status: 401,
			code: 'unauthorized',
			message: 'sign in'
		});
		expect(onUnauthorized).toHaveBeenCalledTimes(1);
		await expect(client.login('nope')).rejects.toBeInstanceOf(ApiError);
		expect(onUnauthorized).toHaveBeenCalledTimes(1);
	});

	it('survives error responses without a JSON body', async () => {
		const client = api(
			() => new Response('bad gateway', { status: 502, statusText: 'Bad Gateway' })
		);
		await expect(client.mailboxes()).rejects.toMatchObject({ status: 502, code: 'http_502' });
	});

	it('builds render URLs', () => {
		expect(renderUrl(5, false)).toBe('/api/messages/5/render?images=0');
		expect(renderUrl(5, true)).toBe('/api/messages/5/render?images=1');
		expect(downloadUrl('/api/parts/abc?exp=1&sig=x')).toBe('/api/parts/abc?exp=1&sig=x&download=1');
		expect(downloadUrl('/api/parts/abc')).toBe('/api/parts/abc?download=1');
	});
});
