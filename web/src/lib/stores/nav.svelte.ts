import type { LabelDto, MailboxDto } from '@tagmail/api-types';
import type { Api } from '../transport';

/** IMAP reserves the name INBOX, case-insensitively, for the inbox. */
export function isInbox(m: MailboxDto): boolean {
	return m.name.toUpperCase() === 'INBOX';
}

/** Mailboxes and labels for the left nav and for mapping change payloads to views. */
export class NavStore {
	mailboxes = $state<MailboxDto[]>([]);
	labels = $state<LabelDto[]>([]);
	loaded = $state(false);
	error = $state<string | null>(null);

	readonly #api: Api;
	#loading: Promise<void> | null = null;

	constructor(api: Api) {
		this.#api = api;
	}

	inboxIds = $derived(new Set(this.mailboxes.filter(isInbox).map((m) => m.id)));

	labelName(key: string): string {
		return this.labels.find((l) => l.key === key)?.name ?? key;
	}

	load(): Promise<void> {
		this.#loading ??= this.#fetch().finally(() => (this.#loading = null));
		return this.#loading;
	}

	async #fetch(): Promise<void> {
		try {
			const [mailboxes, labels] = await Promise.all([this.#api.mailboxes(), this.#api.labels()]);
			this.mailboxes = mailboxes;
			this.labels = [...labels].sort((a, b) => a.name.localeCompare(b.name));
			this.error = null;
		} catch (e) {
			this.error = e instanceof Error ? e.message : String(e);
		} finally {
			this.loaded = true;
		}
	}
}
