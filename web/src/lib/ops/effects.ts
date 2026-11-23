import type { ThreadRowDto } from '@tagmail/api-types';
import type { OpKind, OpRequest, ViewId } from '../transport';

/** An op before it gets its id. */
export type OpIntent = Omit<OpRequest, 'opId'> & { threadIds: number[] };

export interface ViewContext {
	/** Mailbox ids that are an INBOX. */
	inboxIds: ReadonlySet<number>;
}

/** The row as it looks once `intent` applied; ops are idempotent setters. */
export function patchRow(
	row: ThreadRowDto,
	intent: Pick<OpIntent, 'kind' | 'label'>
): ThreadRowDto {
	switch (intent.kind) {
		case 'label.add':
			if (!intent.label || row.labels.includes(intent.label)) return row;
			return { ...row, labels: [...row.labels, intent.label].sort() };
		case 'label.remove':
			if (!intent.label || !row.labels.includes(intent.label)) return row;
			return { ...row, labels: row.labels.filter((l) => l !== intent.label) };
		case 'markRead':
			return row.unreadCount === 0 ? row : { ...row, unreadCount: 0 };
		case 'markUnread':
			return row.unreadCount > 0 ? row : { ...row, unreadCount: 1 };
		case 'star':
			return row.starred ? row : { ...row, starred: true };
		case 'unstar':
			return row.starred ? { ...row, starred: false } : row;
		default:
			return row;
	}
}

const REMOVING: ReadonlySet<OpKind> = new Set(['trash', 'spam', 'delete']);

function mailboxOf(view: ViewId): number | null {
	return view.startsWith('mailbox:') ? Number(view.slice('mailbox:'.length)) : null;
}

function isInboxView(view: ViewId, ctx: ViewContext): boolean {
	if (view === 'inbox') return true;
	const box = mailboxOf(view);
	return box !== null && ctx.inboxIds.has(box);
}

/** Whether the threads of `intent` drop out of `view` right away. */
export function leavesView(intent: OpIntent, view: ViewId, ctx: ViewContext): boolean {
	const box = mailboxOf(view);
	if (REMOVING.has(intent.kind)) return box === null || box !== intent.mailboxId;
	if (isInboxView(view, ctx)) {
		if (intent.kind === 'archive') return true;
		if (intent.kind === 'move') return !ctx.inboxIds.has(intent.mailboxId ?? -1);
		return false;
	}
	if (box !== null) {
		if (intent.kind === 'unarchive') return true;
		if (intent.kind === 'move') return intent.mailboxId !== box;
		return false;
	}
	if (view === `label:${intent.label}`) return intent.kind === 'label.remove';
	return false;
}

/** Whether the threads of `intent` show up in `view` (which then reloads). */
export function joinsView(intent: OpIntent, view: ViewId, ctx: ViewContext): boolean {
	if (intent.kind === 'unarchive') return isInboxView(view, ctx);
	if (intent.kind === 'move') {
		const box = mailboxOf(view);
		return (
			(box !== null && box === intent.mailboxId) ||
			(view === 'inbox' && ctx.inboxIds.has(intent.mailboxId ?? -1))
		);
	}
	if (intent.kind === 'label.add') return view === `label:${intent.label}`;
	return false;
}

/**
 * The op that takes `intent` back, or `null` where the old location is not
 * known (a trash or move started from a label or search list).
 */
export function inverseOf(intent: OpIntent, view: ViewId, ctx: ViewContext): OpIntent | null {
	const base = { threadIds: intent.threadIds };
	switch (intent.kind) {
		case 'label.add':
			return { ...base, kind: 'label.remove', label: intent.label };
		case 'label.remove':
			return { ...base, kind: 'label.add', label: intent.label };
		case 'markRead':
			return { ...base, kind: 'markUnread' };
		case 'markUnread':
			return { ...base, kind: 'markRead' };
		case 'star':
			return { ...base, kind: 'unstar' };
		case 'unstar':
			return { ...base, kind: 'star' };
		case 'archive':
			return { ...base, kind: 'unarchive' };
		case 'unarchive':
			return { ...base, kind: 'archive' };
		case 'trash':
		case 'spam':
		case 'move': {
			if (isInboxView(view, ctx)) return { ...base, kind: 'unarchive' };
			const box = mailboxOf(view);
			return box === null ? null : { ...base, kind: 'move', mailboxId: box };
		}
		case 'delete':
			return null;
	}
}

export interface Names {
	label: (key: string) => string;
	mailbox: (id: number) => string;
}

function conversations(n: number): string {
	return `${n.toLocaleString()} conversation${n === 1 ? '' : 's'}`;
}

/** Past-tense summary for the snackbar: "Archived 3 conversations". */
export function describeDone(intent: OpIntent, names: Names): string {
	const what = conversations(intent.threadIds.length);
	const label = intent.label ? names.label(intent.label) : '';
	switch (intent.kind) {
		case 'label.add':
			return `Added “${label}” to ${what}`;
		case 'label.remove':
			return `Removed “${label}” from ${what}`;
		case 'archive':
			return `Archived ${what}`;
		case 'unarchive':
			return `Moved ${what} to Inbox`;
		case 'move':
			return `Moved ${what} to ${names.mailbox(intent.mailboxId ?? -1)}`;
		case 'markRead':
			return `Marked ${what} as read`;
		case 'markUnread':
			return `Marked ${what} as unread`;
		case 'star':
			return `Starred ${what}`;
		case 'unstar':
			return `Unstarred ${what}`;
		case 'trash':
			return `Moved ${what} to Trash`;
		case 'spam':
			return `Reported ${what} as spam`;
		case 'delete':
			return `Deleted ${what} forever`;
	}
}

/** Imperative summary for failures and the unsynced list: "Archive 3 conversations". */
export function describeIntent(intent: OpIntent, names: Names): string {
	const what = conversations(intent.threadIds.length);
	const label = intent.label ? names.label(intent.label) : '';
	switch (intent.kind) {
		case 'label.add':
			return `Add “${label}” to ${what}`;
		case 'label.remove':
			return `Remove “${label}” from ${what}`;
		case 'archive':
			return `Archive ${what}`;
		case 'unarchive':
			return `Move ${what} to Inbox`;
		case 'move':
			return `Move ${what} to ${names.mailbox(intent.mailboxId ?? -1)}`;
		case 'markRead':
			return `Mark ${what} as read`;
		case 'markUnread':
			return `Mark ${what} as unread`;
		case 'star':
			return `Star ${what}`;
		case 'unstar':
			return `Unstar ${what}`;
		case 'trash':
			return `Move ${what} to Trash`;
		case 'spam':
			return `Report ${what} as spam`;
		case 'delete':
			return `Delete ${what} forever`;
	}
}
