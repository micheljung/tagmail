import type { AddressDto } from '@tagmail/api-types';
import {
	ATTACHMENT_MAX_BYTES,
	DRAFT_ATTACHMENTS_MAX_BYTES,
	type AttachmentDto,
	type DraftInputDto,
	type ReplyMode
} from '../transport';
import { fileSize } from '../format';
import { parseAddresses, toRecipient, type Recipient } from './addresses';

export const AUTOSAVE_MS = 1200;

export interface DraftApi {
	createDraft(draft: DraftInputDto, keepalive?: boolean): Promise<{ id: number }>;
	updateDraft(id: number, draft: DraftInputDto, keepalive?: boolean): Promise<unknown>;
	deleteDraft(id: number): Promise<void>;
	uploadAttachment(
		file: File,
		onProgress: (loaded: number, total: number) => void,
		signal?: AbortSignal
	): Promise<AttachmentDto>;
	removeAttachment(id: string): Promise<void>;
}

export interface Upload {
	key: number;
	name: string;
	size: number;
	loaded: number;
	status: 'uploading' | 'done' | 'error';
	error: string | null;
	id: string | null;
	controller: AbortController | null;
}

export type SaveState = 'idle' | 'saving' | 'saved' | 'error';

export interface DraftInit {
	identityId?: number | null;
	to?: AddressDto[];
	cc?: AddressDto[];
	bcc?: AddressDto[];
	subject?: string;
	html?: string | null;
	inReplyTo?: number | null;
	forwardOf?: number | null;
	attachments?: AttachmentDto[];
	mode?: ReplyMode | 'new';
	draftId?: number | null;
}

let uploadKeys = 1;

/**
 * One message being written: fields, attachments and a debounced autosave
 * to `/api/drafts` (create once, then replace).
 */
export class Draft {
	identityId = $state<number | null>(null);
	to = $state<Recipient[]>([]);
	cc = $state<Recipient[]>([]);
	bcc = $state<Recipient[]>([]);
	showCc = $state(false);
	showBcc = $state(false);
	subject = $state('');
	html = $state('');
	text = $state('');
	uploads = $state<Upload[]>([]);
	draftId = $state<number | null>(null);
	saveState = $state<SaveState>('idle');
	saveError = $state<string | null>(null);
	readonly mode: ReplyMode | 'new';
	readonly inReplyTo: number | null;
	readonly forwardOf: number | null;

	readonly #api: DraftApi;
	#timer: ReturnType<typeof setTimeout> | null = null;
	#saving: Promise<void> | null = null;
	#again = false;
	#closed = false;
	#dirty = false;
	/** The server holds edits it has not written to the IMAP Drafts mailbox yet. */
	#imapBehind = false;
	#againNow = false;

	constructor(api: DraftApi, init: DraftInit = {}) {
		this.#api = api;
		this.identityId = init.identityId ?? null;
		this.to = (init.to ?? []).map(toRecipient);
		this.cc = (init.cc ?? []).map(toRecipient);
		this.bcc = (init.bcc ?? []).map(toRecipient);
		this.showCc = this.cc.length > 0;
		this.showBcc = this.bcc.length > 0;
		this.subject = init.subject ?? '';
		this.html = init.html ?? '';
		this.mode = init.mode ?? 'new';
		this.inReplyTo = init.inReplyTo ?? null;
		this.forwardOf = init.forwardOf ?? null;
		this.draftId = init.draftId ?? null;
		this.uploads = (init.attachments ?? []).map((a) => ({
			key: uploadKeys++,
			name: a.name,
			size: a.size,
			loaded: a.size,
			status: 'done',
			error: null,
			id: a.id,
			controller: null
		}));
	}

	invalidRecipients = $derived([...this.to, ...this.cc, ...this.bcc].filter((r) => !r.valid));
	hasRecipients = $derived(this.to.length + this.cc.length + this.bcc.length > 0);
	uploading = $derived(this.uploads.some((u) => u.status === 'uploading'));
	attachedBytes = $derived(
		this.uploads.filter((u) => u.status !== 'error').reduce((n, u) => n + u.size, 0)
	);

	/** Why it can't be sent yet, or `null`. */
	sendBlocker = $derived.by(() => {
		if (!this.hasRecipients) return 'Add at least one recipient';
		const bad = this.invalidRecipients[0];
		if (bad) return `Check the address “${bad.email}”`;
		if (this.uploading) return 'Wait for the attachments to finish uploading';
		if (this.identityId === null) return 'Choose who it is from';
		return null;
	});

	isEmpty(): boolean {
		return (
			!this.hasRecipients && !this.subject.trim() && !this.text.trim() && this.uploads.length === 0
		);
	}

	/** Field edits call this; the save follows once typing pauses. */
	touch(): void {
		if (this.#closed) return;
		this.#dirty = true;
		if (this.#timer) clearTimeout(this.#timer);
		this.#timer = setTimeout(() => {
			this.#timer = null;
			void this.save();
		}, AUTOSAVE_MS);
	}

	addRecipients(field: 'to' | 'cc' | 'bcc', text: string): void {
		const parsed = parseAddresses(text);
		if (parsed.length === 0) return;
		const known = new Set(this[field].map((r) => r.email.toLowerCase()));
		const fresh = parsed.filter((r) => {
			const email = r.email.toLowerCase();
			if (known.has(email)) return false;
			known.add(email);
			return true;
		});
		this[field] = [...this[field], ...fresh];
		this.touch();
	}

	removeRecipient(field: 'to' | 'cc' | 'bcc', index: number): void {
		this[field] = this[field].filter((_, i) => i !== index);
		this.touch();
	}

	input(): DraftInputDto {
		const plain = (list: Recipient[]) => list.map(({ name, email }) => ({ name, email }));
		const body: DraftInputDto = {
			identityId: this.identityId ?? 0,
			to: plain(this.to),
			cc: plain(this.cc),
			bcc: plain(this.bcc),
			subject: this.subject,
			text: this.text,
			html: this.html,
			attachmentIds: this.uploads.filter((u) => u.id !== null).map((u) => u.id as string)
		};
		if (this.inReplyTo !== null) body.inReplyTo = this.inReplyTo;
		if (this.forwardOf !== null) body.forwardOf = this.forwardOf;
		return body;
	}

	/**
	 * Saves now (coalescing with a save in flight); resolves once stored.
	 * `imap` asks the server to write it to the Drafts mailbox right away
	 * instead of after its idle delay; `keepalive` lets the request outlive
	 * the page.
	 */
	async save(options: { imap?: boolean; keepalive?: boolean } = {}): Promise<void> {
		if (this.#timer) {
			clearTimeout(this.#timer);
			this.#timer = null;
		}
		const imap = options.imap ?? false;
		if (this.#saving) {
			this.#again = true;
			this.#againNow ||= imap;
			return this.#saving;
		}
		const needed =
			this.draftId === null ? !this.isEmpty() : this.#dirty || (imap && this.#imapBehind);
		if (!needed) return;
		this.#saving = this.#store(imap, options.keepalive ?? false).finally(
			() => (this.#saving = null)
		);
		await this.#saving;
		if (this.#again) {
			const again = { imap: this.#againNow };
			this.#again = false;
			this.#againNow = false;
			await this.save(again);
		}
	}

	async #store(imap: boolean, keepalive: boolean): Promise<void> {
		this.saveState = 'saving';
		this.#dirty = false;
		try {
			const body = this.input();
			if (imap) body.saveNow = true;
			if (this.draftId === null) {
				this.draftId = (await this.#api.createDraft(body, keepalive)).id;
			} else await this.#api.updateDraft(this.draftId, body, keepalive);
			this.#imapBehind = !imap;
			this.saveState = 'saved';
			this.saveError = null;
		} catch (e) {
			this.#dirty = true;
			this.saveState = 'error';
			this.saveError = e instanceof Error ? e.message : String(e);
			throw e;
		}
	}

	/** The page is going away or hidden: store everything, Drafts mailbox included. */
	flush(): void {
		if (this.#closed) return;
		void this.save({ imap: true, keepalive: true }).catch(() => undefined);
	}

	/** Uploads files after checking the caps; rejected files show their reason. */
	attach(files: Iterable<File>): void {
		let total = this.attachedBytes;
		for (const file of files) {
			const upload: Upload = {
				key: uploadKeys++,
				name: file.name,
				size: file.size,
				loaded: 0,
				status: 'uploading',
				error: null,
				id: null,
				controller: null
			};
			if (file.size > ATTACHMENT_MAX_BYTES) {
				upload.status = 'error';
				upload.error = `Larger than ${fileSize(ATTACHMENT_MAX_BYTES)}`;
			} else if (total + file.size > DRAFT_ATTACHMENTS_MAX_BYTES) {
				upload.status = 'error';
				upload.error = `Attachments would exceed ${fileSize(DRAFT_ATTACHMENTS_MAX_BYTES)} in total`;
			} else {
				total += file.size;
				upload.controller = new AbortController();
			}
			this.uploads = [...this.uploads, upload];
			if (upload.status === 'uploading') void this.#upload(upload.key, file);
		}
	}

	#patchUpload(key: number, patch: Partial<Upload>): void {
		this.uploads = this.uploads.map((u) => (u.key === key ? { ...u, ...patch } : u));
	}

	async #upload(key: number, file: File): Promise<void> {
		const signal = this.uploads.find((u) => u.key === key)?.controller?.signal;
		try {
			const dto = await this.#api.uploadAttachment(
				file,
				(loaded) => this.#patchUpload(key, { loaded }),
				signal
			);
			this.#patchUpload(key, { status: 'done', loaded: file.size, id: dto.id, controller: null });
			this.touch();
		} catch (e) {
			if (signal?.aborted) return;
			const message = e instanceof Error ? e.message : String(e);
			this.#patchUpload(key, { status: 'error', error: message, controller: null });
		}
	}

	removeUpload(key: number): void {
		const upload = this.uploads.find((u) => u.key === key);
		if (!upload) return;
		upload.controller?.abort();
		this.uploads = this.uploads.filter((u) => u.key !== key);
		if (upload.id) void this.#api.removeAttachment(upload.id).catch(() => undefined);
		this.touch();
	}

	/**
	 * Stops autosave after a last save that goes to the Drafts mailbox at
	 * once; with `discard`, deletes the stored draft instead.
	 */
	async close(discard = false): Promise<void> {
		if (this.#timer) clearTimeout(this.#timer);
		this.#timer = null;
		if (!discard) {
			await this.save({ imap: true }).catch(() => undefined);
			this.#closed = true;
			return;
		}
		this.#closed = true;
		for (const u of this.uploads) u.controller?.abort();
		await this.#saving?.catch(() => undefined);
		if (this.draftId !== null) await this.#api.deleteDraft(this.draftId).catch(() => undefined);
	}

	/** Stops autosave once the draft is queued for sending (the server refuses edits then). */
	finish(): void {
		if (this.#timer) clearTimeout(this.#timer);
		this.#timer = null;
		this.#closed = true;
	}
}
