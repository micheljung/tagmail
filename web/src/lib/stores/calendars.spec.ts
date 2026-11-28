import type { ApiTransport, CalendarDto } from '@tagmail/api-types';
import { describe, expect, it } from 'vitest';
import { Api } from '../transport/api';
import { CalendarsStore } from './calendars.svelte';

const WORK: CalendarDto = {
	accountId: 1,
	href: '/dav/work/',
	displayName: 'Work',
	color: '#1d7a4f',
	visible: true,
	sort: null,
	writable: true,
	isDefault: false
};

function store(server: { calendars: CalendarDto[]; fail: boolean; listFails?: boolean }) {
	const transport: ApiTransport = {
		request: (path, init) => {
			if (init?.method === 'PATCH') {
				return Promise.resolve(
					server.fail
						? new Response(JSON.stringify({ error: 'offline', message: 'no' }), { status: 503 })
						: new Response(JSON.stringify(server.calendars[0]))
				);
			}
			if (server.listFails) return Promise.reject(new TypeError('offline'));
			return Promise.resolve(new Response(JSON.stringify({ calendars: server.calendars })));
		},
		events: () => () => {}
	};
	return new CalendarsStore(new Api(transport));
}

describe('CalendarsStore', () => {
	it('shows a change before the server answers', async () => {
		const server = { calendars: [{ ...WORK }], fail: false };
		const calendars = store(server);
		await calendars.load();
		const patched = calendars.patch({ accountId: 1, href: WORK.href, visible: false });
		expect(calendars.calendars[0].visible).toBe(false);
		server.calendars[0].visible = false;
		await patched;
		expect(calendars.calendars[0].visible).toBe(false);
	});

	it('keeps a saved change when only the reload fails', async () => {
		const server = { calendars: [{ ...WORK }], fail: false, listFails: false };
		const calendars = store(server);
		await calendars.load();
		server.listFails = true;
		await calendars.patch({ accountId: 1, href: WORK.href, visible: false });
		expect(calendars.calendars[0].visible).toBe(false);
	});

	it('restores the previous values when the change fails', async () => {
		const server = { calendars: [{ ...WORK }], fail: true, listFails: false };
		const calendars = store(server);
		await calendars.load();
		server.listFails = true;
		await expect(
			calendars.patch({ accountId: 1, href: WORK.href, color: '#000000' })
		).rejects.toThrow();
		expect(calendars.calendars[0].color).toBe('#1d7a4f');
	});
});
