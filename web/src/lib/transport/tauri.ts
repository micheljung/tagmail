import type { ApiErrorDto, ApiEvent, ApiTransport, ChangesDto } from '@tagmail/api-types';
import { HttpTransport } from './http';

export type Unlisten = () => void;
export type Listen = <T>(event: string, handler: (payload: T) => void) => Promise<Unlisten>;

/** True inside the Tauri desktop app, which injects this global. */
export function isDesktop(): boolean {
	return '__TAURI_INTERNALS__' in globalThis;
}

const tauriListen: Listen = async (event, handler) => {
	const { listen } = await import('@tauri-apps/api/event');
	const unlisten = await listen(event, (e) => handler(e.payload as never));
	return () => void unlisten();
};

const GONE: ApiErrorDto = {
	error: 'gone',
	message: 'the event stream closed; reload the views'
};

/**
 * Desktop transport: the app serves the SPA and `/api` on one custom
 * protocol origin, so requests are plain same-origin `fetch`. Responses
 * there cannot stream, so changes arrive as Tauri events after
 * `/api/desktop/events/start`.
 */
export class TauriTransport implements ApiTransport {
	readonly #http: HttpTransport;
	readonly #listen: Listen;

	constructor(options: { fetch?: typeof fetch; listen?: Listen } = {}) {
		this.#http = new HttpTransport({ fetch: options.fetch });
		this.#listen = options.listen ?? tauriListen;
	}

	request(path: string, init?: RequestInit): Promise<Response> {
		return this.#http.request(path, init);
	}

	/** Desktop-only events such as `desktop:status` or `desktop:mailto`. */
	listen<T>(event: string, handler: (payload: T) => void): Promise<Unlisten> {
		return this.#listen(event, handler);
	}

	events(since: string | undefined, onEvent: (event: ApiEvent) => void): () => void {
		let open = true;
		const unlisteners: Unlisten[] = [];
		const stop = () => {
			open = false;
			for (const unlisten of unlisteners.splice(0)) unlisten();
		};
		const reset = (data: ApiErrorDto) => {
			if (!open) return;
			stop();
			onEvent({ type: 'reset', data });
		};
		void (async () => {
			unlisteners.push(
				await this.#listen<ChangesDto>('api:changes', (data) => {
					if (open) onEvent({ type: 'changes', id: data.next, data });
				}),
				await this.#listen<ApiErrorDto>('api:reset', reset)
			);
			if (!open) return stop();
			const query = since ? `?since=${encodeURIComponent(since)}` : '';
			const resp = await this.request(`/api/desktop/events/start${query}`);
			if (!resp.ok) reset(GONE);
		})().catch(() => reset(GONE));
		return stop;
	}
}
