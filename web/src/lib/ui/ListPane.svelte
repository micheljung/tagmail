<script lang="ts">
	import { listMeta } from '../listMeta';
	import type { ListTarget } from '../readingPane';
	import { listNames, lists, prefs } from '../stores';
	import { viewScope } from '../views';
	import ThreadListView from './ThreadListView.svelte';

	let { target, routeId }: { target: ListTarget; routeId: number | null } = $props();

	// The (lists) layout keys this component on the view and inbox type, so the initial target is the only one.
	// svelte-ignore state_referenced_locally
	const { view, path } = target;
	const { inner } = viewScope(view);
	const list = lists.open(view, path, {
		unreadFirst: inner === 'inbox' && prefs.inboxType === 'unread'
	});
	const meta = $derived(listMeta(view, listNames));
	const hiddenLabel = inner.startsWith('label:') ? inner.slice('label:'.length) : undefined;
</script>

<ThreadListView
	{list}
	label={meta.label}
	emptyTitle={meta.emptyTitle}
	emptyHint={meta.emptyHint}
	{hiddenLabel}
	deferToThread={routeId !== null}
	{routeId}
/>
