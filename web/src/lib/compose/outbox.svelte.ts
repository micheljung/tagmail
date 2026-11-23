import type { OutboxItemDto, OutboxStatus, SendAckDto } from '../transport';
import type { Notifier } from '../ops/ops.svelte';

export const OUTBOX_POLL_MS = 700;

export interface OutboxApi {
	send(opId: string, draftId: number): Promise<SendAckDto>;
	outbox(): Promise<OutboxItemDto[]>;
	cancelOutbox(id: number): Promise<void>;
}

export interface Sending {
	opId: string;
	draftId: number;
	subject: string;
	status: OutboxStatus | 'sent';
	error: string | null;
	/** Outbox row id once the server listed it. */
	id: number | null;
}

/**
 * Sent messages until the server confirms them: "Sending…" while queued,
 * "Message sent" once the outbox no longer lists it, Retry on failure.
 */
export class Outbox {
	items = $state.raw<Sending[]>([]);

	readonly #api: OutboxApi;
	readonly #notify: Notifier;
	readonly #reopen: (draftId: number) => void;
	readonly #uuid: () => string;
	#timer: ReturnType<typeof setTimeout> | null = null;

	constructor(
		api: OutboxApi,
		notify: Notifier,
		reopen: (draftId: number) => void,
		uuid: () => string = () => crypto.randomUUID()
	) {
		this.#api = api;
		this.#notify = notify;
		this.#reopen = reopen;
		this.#uuid = uuid;
	}

	get failed(): Sending[] {
		return this.items.filter((i) => i.status === 'failed');
	}

	async send(draftId: number, subject: string): Promise<void> {
		const item: Sending = {
			opId: this.#uuid(),
			draftId,
			subject,
			status: 'queued',
			error: null,
			id: null
		};
		this.items = [...this.items.filter((i) => i.draftId !== draftId), item];
		await this.#post(item);
	}

	async retry(item: Sending): Promise<void> {
		this.items = this.items.filter((i) => i !== item);
		await this.send(item.draftId, item.subject);
	}

	async cancel(item: Sending): Promise<void> {
		if (item.id === null || item.status === 'sending') return;
		try {
			await this.#api.cancelOutbox(item.id);
			this.items = this.items.filter((i) => i !== item);
			this.#notify.dismissKey(`send:${item.opId}`);
			this.#reopen(item.draftId);
		} catch (e) {
			this.#notify.show(`Too late to cancel: ${e instanceof Error ? e.message : String(e)}`, {
				tone: 'error'
			});
		}
	}

	dismiss(item: Sending): void {
		this.items = this.items.filter((i) => i !== item);
		this.#notify.dismissKey(`send:${item.opId}`);
	}

	/** Failed sends from an earlier visit. */
	async load(): Promise<void> {
		const rows = await this.#api.outbox();
		const known = new Set(this.items.map((i) => i.opId));
		const restored = rows
			.filter((r) => r.status === 'failed' && !known.has(r.opId))
			.map((r) => ({
				opId: r.opId,
				draftId: r.draftId,
				subject: r.subject,
				status: r.status,
				error: r.error ?? null,
				id: r.id
			}));
		if (restored.length > 0) this.items = [...this.items, ...restored];
	}

	async #post(item: Sending): Promise<void> {
		this.#progress(item);
		try {
			await this.#api.send(item.opId, item.draftId);
		} catch (e) {
			this.#update(item.opId, {
				status: 'failed',
				error: e instanceof Error ? e.message : String(e)
			});
			return;
		}
		this.#schedule();
	}

	#progress(item: Sending): void {
		this.#notify.show('Sending…', {
			tone: 'progress',
			key: `send:${item.opId}`,
			duration: 0,
			actions:
				item.id !== null && item.status === 'queued'
					? [{ label: 'Cancel', run: () => void this.cancel(this.#find(item.opId) ?? item) }]
					: []
		});
	}

	#find(opId: string): Sending | undefined {
		return this.items.find((i) => i.opId === opId);
	}

	#update(opId: string, patch: Partial<Sending>): void {
		const before = this.#find(opId);
		if (!before) return;
		const after = { ...before, ...patch };
		this.items = this.items.map((i) => (i.opId === opId ? after : i));
		if (after.status === 'failed' && before.status !== 'failed') {
			this.#notify.show(`Couldn't send “${after.subject || '(no subject)'}”`, {
				tone: 'error',
				key: `send:${opId}`,
				duration: 0,
				actions: [{ label: 'Retry', run: () => void this.retry(after) }]
			});
		} else if (after.status === 'sent') {
			this.items = this.items.filter((i) => i.opId !== opId);
			this.#notify.show('Message sent', { key: `send:${opId}` });
		} else if (before.id === null && after.id !== null) {
			this.#progress(after);
		}
	}

	#schedule(): void {
		if (this.#timer) return;
		this.#timer = setTimeout(() => {
			this.#timer = null;
			void this.#poll();
		}, OUTBOX_POLL_MS);
	}

	async #poll(): Promise<void> {
		const active = this.items.filter((i) => i.status === 'queued' || i.status === 'sending');
		if (active.length === 0) return;
		let rows: OutboxItemDto[];
		try {
			rows = await this.#api.outbox();
		} catch {
			this.#schedule();
			return;
		}
		const byOp = new Map(rows.map((r) => [r.opId, r]));
		for (const item of active) {
			const row = byOp.get(item.opId);
			if (!row) this.#update(item.opId, { status: 'sent' });
			else this.#update(item.opId, { status: row.status, error: row.error ?? null, id: row.id });
		}
		if (this.items.some((i) => i.status === 'queued' || i.status === 'sending')) this.#schedule();
	}
}
