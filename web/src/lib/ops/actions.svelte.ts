import type { ThreadRowDto } from '@tagmail/api-types';
import type { ShortcutAction } from '../shortcuts';
import { nav, ops } from '../stores';
import type { ViewId } from '../transport';
import type { OpIntent } from './effects';
import { byAccount } from './split';

export interface ActionTarget {
	threadIds: number[];
	/** Rows of those threads as shown (projected), for tri-state labels and star toggles. */
	rows: ThreadRowDto[];
	/** The list the action starts from, for undo. */
	view: ViewId | null;
}

/** Open label (`l`) and move (`v`) pickers; the shell renders them. */
class Pickers {
	label = $state.raw<ActionTarget | null>(null);
	move = $state.raw<ActionTarget | null>(null);
}

export const pickers = new Pickers();

/** The view's mailbox id when it is the Trash, where delete is permanent. */
export function inTrash(view: ViewId | null): boolean {
	if (!view?.startsWith('mailbox:')) return false;
	const id = Number(view.slice('mailbox:'.length));
	return nav.mailboxes.some((m) => m.id === id && m.specialUse === 'trash');
}

/** Where the rows' threads live now, so a move can be undone from any list. */
export function originsOf(rows: readonly ThreadRowDto[]): Map<number, number[]> {
	return new Map(rows.map((r) => [r.id, r.mailboxIds]));
}

/** Runs `intent` on `target`'s threads as one op per account. */
export function runOp(
	intent: Omit<OpIntent, 'threadIds'>,
	target: ActionTarget,
	quiet = false
): void {
	for (const threadIds of byAccount(target.threadIds, target.rows)) {
		ops.run(
			{ ...intent, threadIds },
			{ view: target.view, origins: originsOf(target.rows), quiet }
		);
	}
}

/** Runs a write shortcut or toolbar action on `target`; returns whether it applied. */
export function act(
	action: ShortcutAction,
	target: ActionTarget,
	options: { quiet?: boolean } = {}
): boolean {
	const { threadIds, rows, view } = target;
	if (threadIds.length === 0) return false;
	const run = (kind: OpIntent['kind']) => runOp({ kind }, target, options.quiet);
	switch (action) {
		case 'archive':
			run('archive');
			return true;
		case 'delete':
			run(inTrash(view) ? 'delete' : 'trash');
			return true;
		case 'star':
			run(rows.length > 0 && rows.every((r) => r.starred) ? 'unstar' : 'star');
			return true;
		case 'markRead':
			run('markRead');
			return true;
		case 'markUnread':
			run('markUnread');
			return true;
		case 'label':
			pickers.label = target;
			return true;
		case 'move':
			pickers.move = target;
			return true;
		default:
			return false;
	}
}
