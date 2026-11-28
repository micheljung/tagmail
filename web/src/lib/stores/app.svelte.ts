import type { ReminderDto } from '@tagmail/api-types';
import type { ListNames } from '../listMeta';
import { OpsStore } from '../ops/ops.svelte';
import { closeReminders } from '../push/client';
import { appRegistration } from '../pwa/register';
import { api } from '../transport';
import { errorText } from '../ui/errors';
import { accountParam, viewScope } from '../views';
import { AccountsStore } from './accounts.svelte';
import { CalendarsStore } from './calendars.svelte';
import { EventHub } from './events.svelte';
import { NavStore } from './nav.svelte';
import { ThreadList, ThreadLists } from './threadList.svelte';
import { Toasts } from './toasts.svelte';

export const hub = new EventHub(api.transport);
export const nav = new NavStore(api);
export const accounts = new AccountsStore(api);
export const calendars = new CalendarsStore(api);
export const lists: ThreadLists = new ThreadLists(
	(view, options): ThreadList =>
		new ThreadList(view, api, hub, nav, {
			...options,
			unreadIntents: () => ops.unreadIntents(),
			touches: (id) => ops.touches(id)
		})
);
export const toasts = new Toasts();
export const listNames: ListNames = {
	account: (id) => accounts.name(id),
	label: (key) => nav.labelName(key),
	mailbox: (id) => ({
		name: nav.mailboxName(id),
		specialUse: nav.mailboxes.find((m) => m.id === id)?.specialUse ?? null
	})
};
export const ops: OpsStore = new OpsStore({
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
		const dismissed = page.changes
			.filter((c) => c.entity === 'reminder' && c.op.kind === 'delete')
			.map((c) => (c.payload as ReminderDto | null)?.alarmKey)
			.filter((key) => key !== undefined);
		if (dismissed.length > 0) void closeReminders(dismissed, appRegistration);
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
