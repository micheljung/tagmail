import { hub, toasts } from '../stores';
import { api, type IdentityDto, type ReplyMode } from '../transport';
import { errorText } from '../ui/errors';
import { Draft, type DraftInit } from './draft.svelte';
import { Outbox } from './outbox.svelte';

/** At most this many compose windows stay docked; Gmail stacks a few too. */
export const MAX_WINDOWS = 3;

class Composer {
	/** Docked new-message windows, oldest first. */
	windows = $state.raw<Draft[]>([]);
	identities = $state.raw<IdentityDto[]>([]);
	#identities: Promise<IdentityDto[]> | null = null;
	readonly #sending = new WeakSet<Draft>();

	loadIdentities(): Promise<IdentityDto[]> {
		this.#identities ??= api.identities().then(
			(list) => (this.identities = list),
			(e: unknown) => {
				this.#identities = null;
				throw e;
			}
		);
		return this.#identities;
	}

	defaultIdentity(): number | null {
		const list = this.identities;
		return (list.find((i) => i.isDefault) ?? list[0])?.id ?? null;
	}

	newDraft(init: DraftInit = {}): Draft {
		const draft = new Draft(api, init);
		if (draft.identityId === null) {
			draft.identityId = this.defaultIdentity();
			if (draft.identityId === null) {
				void this.loadIdentities()
					.then(() => (draft.identityId ??= this.defaultIdentity()))
					.catch(() => undefined);
			}
		}
		return draft;
	}

	open(init: DraftInit = {}): Draft {
		const draft = this.newDraft(init);
		this.windows = [...this.windows, draft].slice(-MAX_WINDOWS);
		return draft;
	}

	/** Prefilled reply, reply-all or forward of message `messageId`. */
	async reply(messageId: number, mode: ReplyMode): Promise<Draft> {
		await this.loadIdentities().catch(() => undefined);
		const r = await api.replyDraft(messageId, mode);
		return this.newDraft({ ...r, mode });
	}

	async reopen(draftId: number): Promise<void> {
		try {
			await this.loadIdentities().catch(() => undefined);
			const d = await api.draft(draftId);
			this.open({
				identityId: d.identityId,
				to: d.to,
				cc: d.cc,
				bcc: d.bcc,
				subject: d.subject,
				html: d.html,
				attachments: d.attachments,
				draftId: d.id
			});
		} catch (e) {
			toasts.show(`Couldn't reopen the draft: ${errorText(e)}`, { tone: 'error' });
		}
	}

	async close(draft: Draft, discard = false): Promise<void> {
		this.windows = this.windows.filter((d) => d !== draft);
		await draft.close(discard);
		if (discard) toasts.show('Draft discarded');
	}

	/** Saves, queues it in the outbox and closes the window. */
	async send(draft: Draft): Promise<boolean> {
		if (draft.sendBlocker || this.#sending.has(draft)) return false;
		this.#sending.add(draft);
		try {
			await draft.save();
		} catch (e) {
			this.#sending.delete(draft);
			toasts.show(`Couldn't save before sending: ${errorText(e)}`, { tone: 'error' });
			return false;
		}
		if (draft.draftId === null) {
			this.#sending.delete(draft);
			return false;
		}
		this.windows = this.windows.filter((d) => d !== draft);
		await draft.close();
		await outbox.send(draft.draftId, draft.subject);
		return true;
	}
}

export const composer = new Composer();
export const outbox = new Outbox(api, toasts, (id) => void composer.reopen(id));

hub.subscribe({
	changes: (page) => outbox.changes(page),
	async reset() {
		await outbox.load().catch(() => undefined);
		return undefined;
	}
});
