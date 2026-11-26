<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { listMeta } from '#lib/listMeta.ts';
	import { listNames } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';

	const id = $derived(Number(page.params.id));
	const view = $derived(`mailbox:${id}` as const);
	const meta = $derived(listMeta(view, listNames));
</script>

<svelte:head>
	<title>{meta.title} · {APP_NAME}</title>
</svelte:head>

{#key id}
	<ListPage
		{view}
		path={`/mailbox/${id}`}
		label={meta.label}
		emptyTitle={meta.emptyTitle}
		emptyHint={meta.emptyHint}
	/>
{/key}
