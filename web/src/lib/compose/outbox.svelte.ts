import type { ChangesDto } from '@tagmail/api-types';
import type { Notifier } from '../ops/ops.svelte';
import type { OutboxItemDto, OutboxStateDto, SendStatusDto } from '../transport';

export interface OutboxApi {
	send(body: { opId: string; draftId: number }): Promise<SendStatusDto>;
	outbox(): Promise<OutboxItemDto[]>;
	cancelOutbox(id: number): Promise<OutboxItemDto>;
	retryOutbox(id: number): Promise<OutboxItemDto>;
}

export interface Sending {
	opId: string;
	draftId: number | null;
	subject: string;
	status: OutboxStateDto;
	error: string | null;
	/** Outbox item id once the server listed it. */
	id: number | null;
}

/** Payload of an `outbox` change event. */
interface OutboxChange {
	status?: OutboxStateDto;
	error?: string | null;
}

/**
 * Sent messages until the server confirms them: "Sending…" while queued,
 * "Message sent" on the `sent` event, Retry on failure. Status arrives
 * through `outbox` change events; the list is read only to learn item ids.
 */
export class Outbox {
	items = $state.raw<Sending[]>([]);

	readonly #api: OutboxApi;
	readonly #notify: Notifier;
	readonly #reopen: (draftId: number) => void;
	readonly #uuid: () => string;
	#syncing: Promise<void> | null = null;

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
		if (item.id === null) {
			this.#update(item.opId, { status: 'queued', error: null });
			await this.#post({ ...item, status: 'queued' });
			return;
		}
		this.#update(item.opId, { status: 'queued', error: null });
		try {
			const row = await this.#api.retryOutbox(item.id);
			this.#update(item.opId, { status: row.status, error: row.error });
		} catch (e) {
			this.#update(item.opId, { status: 'failed', error: message(e) });
		}
	}

	async cancel(item: Sending): Promise<void> {
		if (item.id === null || item.status !== 'queued') return;
		try {
			await this.#api.cancelOutbox(item.id);
			this.items = this.items.filter((i) => i.opId !== item.opId);
			this.#notify.dismissKey(`send:${item.opId}`);
			if (item.draftId !== null) this.#reopen(item.draftId);
		} catch (e) {
			this.#notify.show(`Too late to cancel: ${message(e)}`, { tone: 'error' });
		}
	}

	/** Forgets a failed send here; the item stays in the server's outbox. */
	dismiss(item: Sending): void {
		this.items = this.items.filter((i) => i.opId !== item.opId);
		this.#notify.dismissKey(`send:${item.opId}`);
	}

	/** Unsent items from an earlier visit; failed ones show in the unsynced panel. */
	load(): Promise<void> {
		this.#syncing ??= this.#sync().finally(() => (this.#syncing = null));
		return this.#syncing;
	}

	changes(page: ChangesDto): void {
		for (const change of page.changes) {
			if (change.entity !== 'outbox') continue;
			const item = this.items.find((i) => i.id === change.id);
			if (change.op.kind === 'delete') {
				if (item) this.dismiss(item);
				continue;
			}
			const payload = (change.payload ?? {}) as OutboxChange;
			if (!item) {
				void this.load();
				continue;
			}
			if (payload.status) {
				this.#update(item.opId, { status: payload.status, error: payload.error ?? null });
			}
		}
	}

	async #sync(): Promise<void> {
		const rows = await this.#api.outbox();
		for (const row of rows) {
			const known = this.items.find((i) => i.opId === row.opId);
			if (known) {
				this.#update(row.opId, { id: row.id, status: row.status, error: row.error });
			} else if (row.status === 'failed' || row.status === 'queued' || row.status === 'sending') {
				this.items = [
					...this.items,
					{
						opId: row.opId,
						draftId: row.draftId,
						subject: row.subject,
						status: row.status,
						error: row.error,
						id: row.id
					}
				];
			}
		}
	}

	async #post(item: Sending): Promise<void> {
		this.#progress(item);
		if (item.draftId === null) return;
		try {
			const ack = await this.#api.send({ opId: item.opId, draftId: item.draftId });
			this.#update(item.opId, { status: ack.status });
		} catch (e) {
			this.#update(item.opId, { status: 'failed', error: message(e) });
			return;
		}
		await this.load().catch(() => undefined);
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
		} else if (
			(before.id === null && after.id !== null) ||
			(before.status !== after.status && after.status !== 'failed')
		) {
			this.#progress(after);
		}
	}
}

function message(e: unknown): string {
	return e instanceof Error ? e.message : String(e);
}
