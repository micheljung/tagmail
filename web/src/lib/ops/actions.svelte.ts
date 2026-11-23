import type { ThreadRowDto } from '@tagmail/api-types';
import type { ShortcutAction } from '../shortcuts';
import { nav, ops } from '../stores';
import type { ViewId } from '../transport';

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
function inTrash(view: ViewId | null): boolean {
	if (!view?.startsWith('mailbox:')) return false;
	const id = Number(view.slice('mailbox:'.length));
	return nav.mailboxes.some((m) => m.id === id && m.specialUse === 'trash');
}

/** Runs a write shortcut or toolbar action on `target`; returns whether it applied. */
export function act(action: ShortcutAction, target: ActionTarget): boolean {
	const { threadIds, rows, view } = target;
	if (threadIds.length === 0) return false;
	const run = (kind: Parameters<typeof ops.run>[0]['kind']) =>
		ops.run({ kind, threadIds }, { view });
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
