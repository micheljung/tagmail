import type { AddressDto, LabelDto } from '@tagmail/api-types';

/*
 * Write-path shapes from the M3 API contract. They mirror the server DTOs
 * until ts-rs exports them into @tagmail/api-types.
 */

export type OpKind =
	| 'label.add'
	| 'label.remove'
	| 'archive'
	| 'unarchive'
	| 'move'
	| 'markRead'
	| 'markUnread'
	| 'star'
	| 'unstar'
	| 'trash'
	| 'spam'
	| 'delete';

export interface OpRequest {
	opId: string;
	kind: OpKind;
	threadIds?: number[];
	emailIds?: string[];
	label?: string;
	mailboxId?: number;
}

export type OpStatus = 'pending' | 'done' | 'failed';

export interface OpDto {
	opId: string;
	kind: OpKind;
	status: OpStatus;
	error?: string | null;
	threadIds?: number[];
	label?: string | null;
	mailboxId?: number | null;
	createdAt?: number;
}

export interface OpAckDto {
	opId: string;
	status: OpStatus;
}

export interface LabelUpdateDto {
	label: LabelDto;
	jobId?: number | string | null;
}

export interface LabelJobDto {
	id: number | string;
	label: string;
	kind: 'rename' | 'delete';
	done: number;
	total: number;
	status: 'running' | 'done' | 'failed';
	error?: string | null;
}

export interface IdentityDto {
	id: number;
	accountId: number;
	address: string;
	name: string | null;
	signature?: string | null;
	isDefault: boolean;
}

export interface AttachmentDto {
	id: string;
	name: string;
	size: number;
	contentType: string;
}

export interface DraftInput {
	identityId: number;
	to: AddressDto[];
	cc: AddressDto[];
	bcc: AddressDto[];
	subject: string;
	text?: string;
	html?: string;
	inReplyTo?: number | null;
	forwardOf?: number | null;
	attachmentIds: string[];
}

export interface DraftDto extends DraftInput {
	id: number;
	attachments?: AttachmentDto[];
}

export interface SendAckDto {
	opId: string;
	status: 'queued';
}

export type OutboxStatus = 'queued' | 'sending' | 'failed';

export interface OutboxItemDto {
	id: number;
	opId: string;
	draftId: number;
	subject: string;
	to: AddressDto[];
	status: OutboxStatus;
	error?: string | null;
}

export type ReplyMode = 'reply' | 'all' | 'forward';

/** `GET /api/messages/{id}/reply`: prefilled draft fields. */
export interface ReplyDraftDto {
	identityId: number;
	to: AddressDto[];
	cc: AddressDto[];
	bcc?: AddressDto[];
	subject: string;
	html: string;
	inReplyTo?: number | null;
	forwardOf?: number | null;
	attachments?: AttachmentDto[];
}

/** Per-file and per-draft attachment caps from the contract. */
export const ATTACHMENT_MAX_BYTES = 25 * 1024 * 1024;
export const DRAFT_ATTACHMENTS_MAX_BYTES = 35 * 1024 * 1024;
