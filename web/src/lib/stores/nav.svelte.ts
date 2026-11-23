import type { LabelDto, MailboxDto, SpecialUseDto } from '@tagmail/api-types';
import type { Api } from '../transport';

const MORE_ORDER: readonly SpecialUseDto[] = ['drafts', 'archive', 'all', 'junk', 'trash'];

const SPECIAL_NAMES: Partial<Record<SpecialUseDto, string>> = {
	all: 'All mail',
	archive: 'Archive',
	drafts: 'Drafts',
	junk: 'Spam',
	sent: 'Sent',
	trash: 'Trash'
};

/** How the UI names a mailbox: special-use folders by role, others by their path. */
export function mailboxLabel(m: MailboxDto): string {
	if (isInbox(m)) return 'Inbox';
	return (m.specialUse && SPECIAL_NAMES[m.specialUse]) || m.name;
}

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

	labelColor(key: string): string | null {
		return this.labels.find((l) => l.key === key)?.color ?? null;
	}

	mailboxName(id: number): string {
		const box = this.mailboxes.find((m) => m.id === id);
		return box ? mailboxLabel(box) : 'another folder';
	}

	/** The player's own folders: neither the Inbox nor a special-use mailbox. */
	folders = $derived(
		this.mailboxes
			.filter((m) => !isInbox(m) && m.specialUse === null)
			.sort((a, b) => a.name.localeCompare(b.name))
	);

	/** Special-use mailboxes for the "More" section, in Gmail's order. */
	specials = $derived(
		this.mailboxes
			.filter((m) => m.specialUse !== null && MORE_ORDER.includes(m.specialUse))
			.sort((a, b) => MORE_ORDER.indexOf(a.specialUse!) - MORE_ORDER.indexOf(b.specialUse!))
	);

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
