<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { nav } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';

	const key = $derived(page.params.name ?? '');
	const name = $derived(nav.labelName(key));
</script>

<svelte:head>
	<title>{name} · {APP_NAME}</title>
</svelte:head>

{#key key}
	<ListPage
		view={`label:${key}`}
		path={`/label/${encodeURIComponent(key)}`}
		label={`Label ${name}`}
		emptyTitle={`Nothing labeled ${name}`}
		emptyHint="Conversations with this label show up here."
		hiddenLabel={key}
	/>
{/key}
