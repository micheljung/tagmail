import { redirect } from '@sveltejs/kit';
import { ApiError, api } from '#lib/transport/index.ts';
import type { LayoutLoad } from './$types';

export const load: LayoutLoad = async ({ url }) => {
	try {
		return { session: await api.session() };
	} catch (e) {
		if (e instanceof ApiError && e.status === 401) {
			if (e.code === 'setup_required') redirect(307, '/setup');
			if (e.code === 'unpaired') redirect(307, '/pair');
			const next = url.pathname + url.search;
			redirect(307, `/login?next=${encodeURIComponent(next)}`);
		}
		throw e;
	}
};
