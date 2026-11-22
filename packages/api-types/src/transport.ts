import type { ChangesDto } from './generated/ChangesDto';
import type { ApiErrorDto } from './generated/ApiErrorDto';

/** Pushed events: SSE on the web, Tauri `emit` on the desktop. */
export type ApiEvent =
	| { type: 'changes'; id: string; data: ChangesDto }
	| { type: 'reset'; data: ApiErrorDto };

/**
 * How the SPA reaches the API. Web: `fetch` + `EventSource`. Desktop: the
 * custom protocol for requests and `listen()` for events, because the SSE
 * stream must end in Rust there (docs/dev/spikes.md, spike 4).
 */
export interface ApiTransport {
	request(path: string, init?: RequestInit): Promise<Response>;
	/** Resumes after `since` (a `<epoch>:<seq>` cursor); returns an unsubscribe function. */
	events(since: string | undefined, onEvent: (event: ApiEvent) => void): () => void;
}
