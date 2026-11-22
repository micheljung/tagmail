import { api } from '../transport';
import { EventHub } from './events.svelte';
import { NavStore } from './nav.svelte';
import { ThreadList, ThreadLists } from './threadList.svelte';

export const hub = new EventHub(api.transport);
export const nav = new NavStore(api);
export const lists = new ThreadLists((view) => new ThreadList(view, api, hub, nav));

hub.subscribe({
	changes(page) {
		const navChanged = page.changes.some(
			(c) =>
				c.entity === 'label' ||
				(c.entity === 'mailbox' &&
					(c.op.kind === 'delete' || (c.payload as { name?: string } | null)?.name !== undefined))
		);
		if (navChanged) void nav.load();
	},
	async reset() {
		await nav.load();
		return undefined;
	}
});

/** After logout: the next session must not see the previous one's views. */
export function resetAppState(): void {
	hub.stop();
	lists.clear();
}
