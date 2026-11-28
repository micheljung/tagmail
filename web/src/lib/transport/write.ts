export type {
	AttachmentDto,
	CalOpInfoDto,
	CalendarDto,
	RsvpPartstatDto,
	DraftDto,
	DraftInputDto,
	IdentityDto,
	LabelChangeDto,
	LabelJobDto,
	LabelPatchDto,
	NewLabelDto,
	NewOpDto,
	OpDto,
	OpKindDto,
	OpStatusDto,
	OutboxItemDto,
	OutboxStateDto,
	ReplyDraftDto,
	ReplyMode,
	SendStatusDto
} from '@tagmail/api-types';

/** Per-file and per-draft attachment caps of `POST /api/attachments` and drafts. */
export const ATTACHMENT_MAX_BYTES = 25 * 1024 * 1024;
export const DRAFT_ATTACHMENTS_MAX_BYTES = 35 * 1024 * 1024;
