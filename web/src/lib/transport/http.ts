import type { ApiErrorDto, ApiEvent, ApiTransport, ChangesDto } from '@tagmail/api-types';

export const CSRF_HEADER = 'X-Requested-With';
export const CSRF_VALUE = 'tagmail';

/** Without it the server leaves calendar entities out of the event stream. */
const INCLUDE_CALENDAR = 'calendar';

const SAFE_METHODS = new Set(['GET', 'HEAD']);

export interface EventSourceLike {
	readonly readyState: number;
	addEventListener(type: string, listener: (event: MessageEvent<string>) => void): void;
	addEventListener(type: 'error', listener: (event: Event) => void): void;
	close(): void;
}

export type EventSourceFactory = (url: string) => EventSourceLike;

const CLOSED = 2;

const GONE: ApiErrorDto = {
	error: 'gone',
	message: 'the event stream closed; reload the views'
};

/**
 * Web transport: same-origin `fetch` with the session cookie and
 * `EventSource` for `/api/events`. A stream the browser gave up on (e.g. a
 * `410` after a resume) is reported as a `reset`, so callers refetch.
 */
export class HttpTransport implements ApiTransport {
	readonly #fetch: typeof fetch;
	readonly #eventSource: EventSourceFactory;
	readonly #base: string;
	readonly #onReachable: (reachable: boolean) => void;

	constructor(
		options: {
			fetch?: typeof fetch;
			eventSource?: EventSourceFactory;
			base?: string;
			/** Told after each request whether the server answered at all. */
			onReachable?: (reachable: boolean) => void;
		} = {}
	) {
		this.#fetch = options.fetch ?? ((input, init) => globalThis.fetch(input, init));
		this.#eventSource =
			options.eventSource ?? ((url) => new EventSource(url, { withCredentials: true }));
		this.#base = options.base ?? '';
		this.#onReachable = options.onReachable ?? (() => {});
	}

	request(path: string, init: RequestInit = {}): Promise<Response> {
		const headers = new Headers(init.headers);
		const method = (init.method ?? 'GET').toUpperCase();
		if (!SAFE_METHODS.has(method)) headers.set(CSRF_HEADER, CSRF_VALUE);
		return this.#fetch(this.#base + path, { ...init, headers, credentials: 'same-origin' }).then(
			(response) => {
				this.#onReachable(true);
				return response;
			},
			(error: unknown) => {
				if (error instanceof TypeError) this.#onReachable(false);
				throw error;
			}
		);
	}

	events(since: string | undefined, onEvent: (event: ApiEvent) => void): () => void {
		const params = new URLSearchParams();
		if (since) params.set('since', since);
		params.set('include', INCLUDE_CALENDAR);
		const source = this.#eventSource(`${this.#base}/api/events?${params}`);
		let open = true;
		const stop = () => {
			open = false;
			source.close();
		};
		source.addEventListener('changes', (event: MessageEvent<string>) => {
			if (!open) return;
			onEvent({
				type: 'changes',
				id: event.lastEventId,
				data: JSON.parse(event.data) as ChangesDto
			});
		});
		source.addEventListener('reset', (event: MessageEvent<string>) => {
			if (!open) return;
			stop();
			onEvent({ type: 'reset', data: JSON.parse(event.data) as ApiErrorDto });
		});
		source.addEventListener('error', () => {
			if (!open || source.readyState !== CLOSED) return;
			stop();
			onEvent({ type: 'reset', data: GONE });
		});
		return stop;
	}
}
