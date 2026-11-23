import { OpsStore } from '../ops/ops.svelte';
import { api } from '../transport';
import { errorText } from '../ui/errors';
import { accountParam, viewScope } from '../views';
import { AccountsStore } from './accounts.svelte';
import { EventHub } from './events.svelte';
import { NavStore } from './nav.svelte';
import { ThreadList, ThreadLists } from './threadList.svelte';
import { Toasts } from './toasts.svelte';

export const hub = new EventHub(api.transport);
export const nav = new NavStore(api);
export const accounts = new AccountsStore(api);
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
		const accountChanges = page.changes.filter((c) => c.entity === 'account');
		if (accountChanges.length > 0) void accounts.load();
		for (const c of accountChanges) {
			if (c.op.kind !== 'delete') continue;
			for (const list of lists.all()) {
				const { account } = viewScope(list.view);
				if (account === null || account === c.id) void list.reset();
			}
		}
		if (navChanged || accountChanges.length > 0) void nav.load();
	},
	async reset() {
		await Promise.all([nav.load(), accounts.load(), ops.load().catch(() => undefined)]);
		return undefined;
	}
});

/** The account the player is browsing (from the URL, else the last list), `null` for all. */
export function currentAccount(url: Parameters<typeof accountParam>[0]): number | null {
	return accountParam(url) ?? (lists.current ? viewScope(lists.current.view).account : null);
}

/** After logout: the next session must not see the previous one's views. */
export function resetAppState(): void {
	hub.stop();
	lists.clear();
}
