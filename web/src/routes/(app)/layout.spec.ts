import { isRedirect } from '@sveltejs/kit';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError, api } from '#lib/transport/index.ts';
import { load } from './+layout';

type LoadEvent = Parameters<typeof load>[0];
const event = { url: new URL('https://mail.example.com/thread/7?x=1') } as LoadEvent;

describe('(app) layout load', () => {
	beforeEach(() => vi.restoreAllMocks());

	it('renders the shell without a session when the network is down', async () => {
		vi.spyOn(api, 'session').mockRejectedValue(new TypeError('Failed to fetch'));
		await expect(load(event)).resolves.toEqual({ session: null });
	});

	it('still sends a signed-out visitor to the login page', async () => {
		vi.spyOn(api, 'session').mockRejectedValue(
			new ApiError(401, { error: 'unauthorized', message: 'sign in first' })
		);
		let thrown: unknown;
		try {
			await load(event);
		} catch (e) {
			thrown = e;
		}
		expect(isRedirect(thrown) && thrown.location).toBe('/login?next=%2Fthread%2F7%3Fx%3D1');
	});

	it('rethrows server errors', async () => {
		const failure = new ApiError(500, { error: 'internal', message: 'boom' });
		vi.spyOn(api, 'session').mockRejectedValue(failure);
		await expect(load(event)).rejects.toBe(failure);
	});
});
