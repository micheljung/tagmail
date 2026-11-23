import { OpsStore } from '../ops/ops.svelte';
import { api } from '../transport';
import { errorText } from '../ui/errors';
import { EventHub } from './events.svelte';
import { NavStore } from './nav.svelte';
import { ThreadList, ThreadLists } from './threadList.svelte';
import { Toasts } from './toasts.svelte';

export const hub = new EventHub(api.transport);
export const nav = new NavStore(api);
export const lists = new ThreadLists((view) => new ThreadList(view, api, hub, nav));
export const toasts = new Toasts();
export const ops = new OpsStore({
	api,
	lists: () => lists.all(),
	context: () => ({ inboxIds: nav.inboxIds }),
	names: () => ({
		label: (key) => nav.labelName(key),
		mailbox: (id) => nav.mailboxName(id)
	}),
	notify: toasts,
	errorText
});

hub.subscribe({
	changes(page) {
		ops.changes(page);
		const navChanged = page.changes.some(
			(c) =>
				c.entity === 'label' ||
				(c.entity === 'mailbox' &&
					(c.op.kind === 'delete' || (c.payload as { name?: string } | null)?.name !== undefined))
		);
		if (navChanged) void nav.load();
	},
	async reset() {
		await Promise.all([nav.load(), ops.load().catch(() => undefined)]);
		return undefined;
	}
});

/** After logout: the next session must not see the previous one's views. */
export function resetAppState(): void {
	hub.stop();
	lists.clear();
}
