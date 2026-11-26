import { viewScope } from './views';
import type { ViewId } from './transport';

export interface ListNames {
	account(id: number): string;
	label(key: string): string;
	mailbox(id: number): { name: string; specialUse: string | null };
}

export interface ListMeta {
	/** The page title, without the app name. */
	title: string;
	label: string;
	emptyTitle: string;
	emptyHint: string;
}

const MAILBOX_EMPTY: Record<string, string> = {
	trash: 'Trash is empty',
	junk: 'No spam here',
	drafts: 'No drafts'
};

export function listMeta(view: ViewId, names: ListNames): ListMeta {
	const { account, inner } = viewScope(view);
	const scope = account === null ? '' : ` in ${names.account(account)}`;
	if (inner.startsWith('label:')) {
		const name = names.label(inner.slice('label:'.length));
		return {
			title: name,
			label: `Label ${name}${scope}`,
			emptyTitle: `Nothing labeled ${name}`,
			emptyHint: 'Conversations with this label show up here.'
		};
	}
	if (inner.startsWith('mailbox:')) {
		const { name, specialUse } = names.mailbox(Number(inner.slice('mailbox:'.length)));
		return {
			title: name,
			label: `Folder ${name}`,
			emptyTitle: (specialUse && MAILBOX_EMPTY[specialUse]) || `${name} is empty`,
			emptyHint: 'Conversations in this folder show up here.'
		};
	}
	if (inner.startsWith('search:')) {
		const query = inner.slice('search:'.length);
		return {
			title: `${query ? `${query} · ` : ''}Search`,
			label: `Search results for ${query}${scope}`,
			emptyTitle: 'No conversations match',
			emptyHint: 'Try fewer words, or operators like from:, label: or after:2026/01/01.'
		};
	}
	const label = account === null ? 'Inbox' : `Inbox of ${names.account(account)}`;
	return {
		title: label,
		label,
		emptyTitle: "You're all caught up",
		emptyHint: 'New conversations land here as soon as they sync.'
	};
}
