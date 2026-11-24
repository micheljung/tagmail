import { describe, expect, it } from 'vitest';
import {
	FALLBACK_URL,
	fetchStrategy,
	safeAppUrl,
	shellCacheName,
	shellReferences,
	staleCaches,
	type FetchInfo,
	type FetchStrategy
} from './sw';

const ORIGIN = 'https://mail.example.com';

describe('fetchStrategy', () => {
	const get = (path: string, mode = 'cors'): FetchInfo => ({
		method: 'GET',
		url: path.startsWith('http') ? path : ORIGIN + path,
		mode
	});
	const cases: [string, FetchInfo, FetchStrategy][] = [
		['API reads', get('/api/threads?ids=1'), 'bypass'],
		['API navigations', get('/api/attachments/4/1', 'navigate'), 'bypass'],
		['the render iframe', get('/api/messages/7/render', 'navigate'), 'bypass'],
		['proxied images', get('/api/proxy/img/abc', 'no-cors'), 'bypass'],
		['the event stream', get('/api/events'), 'bypass'],
		['the version probe', get('/api/version'), 'bypass'],
		['the bare API root', get('/api'), 'bypass'],
		['the worker script', get('/service-worker.js'), 'bypass'],
		['health checks', get('/healthz'), 'bypass'],
		['API writes', { method: 'POST', url: `${ORIGIN}/api/ops`, mode: 'cors' }, 'bypass'],
		['other writes', { method: 'PUT', url: `${ORIGIN}/inbox`, mode: 'cors' }, 'bypass'],
		['HEAD requests', { method: 'HEAD', url: `${ORIGIN}/inbox`, mode: 'cors' }, 'bypass'],
		['cross-origin requests', get('https://cdn.example.com/x.js'), 'bypass'],
		['look-alike origins', get('https://mail.example.com.evil.test/inbox', 'navigate'), 'bypass'],
		['app navigations', get('/thread/4711', 'navigate'), 'network-first'],
		['the root navigation', get('/', 'navigate'), 'network-first'],
		['immutable assets', get('/_app/immutable/entry/app.B2.js'), 'cache-first'],
		['the boot script', get('/_app/boot.5b70ccf33df1.js'), 'cache-first'],
		['static files', get('/icons/icon-192.png', 'no-cors'), 'cache-first'],
		['paths that only start like the API', get('/apiary.png', 'no-cors'), 'cache-first']
	];
	it.each(cases)('%s', (_name, request, expected) => {
		expect(fetchStrategy(request, ORIGIN)).toBe(expected);
	});
});

describe('staleCaches', () => {
	it('drops other shell versions and keeps foreign caches', () => {
		const current = shellCacheName('2');
		expect(staleCaches([shellCacheName('1'), current, 'other-app'], current)).toEqual([
			shellCacheName('1')
		]);
	});
});

describe('shellReferences', () => {
	it('lists same-origin scripts and links, never API or foreign URLs', () => {
		const html = `
			<link href="/_app/immutable/entry/start.js" rel="modulepreload">
			<link rel="manifest" href="/manifest.webmanifest" />
			<link href="https://fonts.example.com/a.css" rel="stylesheet">
			<link href="/api/version" rel="prefetch">
			<script src="/_app/boot.abc.js"></script>
			<script src="/_app/boot.abc.js"></script>
			<script>inline()</script>`;
		expect(shellReferences(html, ORIGIN).sort()).toEqual([
			'/_app/boot.abc.js',
			'/_app/immutable/entry/start.js',
			'/manifest.webmanifest'
		]);
	});
});

describe('safeAppUrl', () => {
	it.each([
		['/thread/4711', '/thread/4711'],
		['/inbox?q=a#b', '/inbox?q=a#b'],
		['/settings/notifications', '/settings/notifications']
	])('keeps %s', (url, expected) => {
		expect(safeAppUrl(url, ORIGIN)).toBe(expected);
	});

	it.each([
		'//evil.test/x',
		'/\\evil.test/x',
		'/\t/evil.test/x',
		'https://evil.test/',
		`${ORIGIN}/inbox`,
		'javascript:alert(1)',
		'thread/1',
		'',
		42,
		null,
		undefined
	])('falls back for %j', (url) => {
		expect(safeAppUrl(url, ORIGIN)).toBe(FALLBACK_URL);
	});
});
