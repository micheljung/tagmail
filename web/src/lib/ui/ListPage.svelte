<script lang="ts">
	import { page } from '$app/state';
	import { splitShown } from '../readingPane';
	import { lists, prefs } from '../stores';
	import type { ViewId } from '../transport';
	import ThreadListView from './ThreadListView.svelte';

	let {
		view,
		path,
		label,
		emptyTitle,
		emptyHint,
		hiddenLabel,
		unreadFirst = false
	}: {
		view: ViewId;
		path: string;
		label: string;
		emptyTitle: string;
		emptyHint: string;
		hiddenLabel?: string;
		/** Unread threads first (inbox views); parents key on it too. */
		unreadFirst?: boolean;
	} = $props();

	// Parents key this component on the view, so the initial props are the only ones. With a
	// reading pane the (lists) layout's ListPane shows the list; turning the pane on or off
	// remounts the route, so checking once is enough.
	// svelte-ignore state_referenced_locally
	const list = splitShown(prefs.split, page.url) ? null : lists.open(view, path, { unreadFirst });
</script>

{#if list}
	<ThreadListView {list} {label} {emptyTitle} {emptyHint} {hiddenLabel} />
{/if}
