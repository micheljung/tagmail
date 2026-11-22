<script lang="ts">
	import { lists } from '../stores';
	import type { ViewId } from '../transport';
	import ThreadListView from './ThreadListView.svelte';

	let {
		view,
		path,
		label,
		emptyTitle,
		emptyHint,
		hiddenLabel
	}: {
		view: ViewId;
		path: string;
		label: string;
		emptyTitle: string;
		emptyHint: string;
		hiddenLabel?: string;
	} = $props();

	// Parents key this component on the view, so the initial props are the only ones.
	// svelte-ignore state_referenced_locally
	const list = lists.open(view, path);
</script>

<ThreadListView {list} {label} {emptyTitle} {emptyHint} {hiddenLabel} />
