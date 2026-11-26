<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { listMeta } from '#lib/listMeta.ts';
	import { listNames } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';
	import { accountParam, scopedView, withAccount } from '#lib/views.ts';

	const key = $derived(page.params.name ?? '');
	const account = $derived(accountParam(page.url));
	const view = $derived(scopedView(`label:${key}`, account));
	const meta = $derived(listMeta(view, listNames));
</script>

<svelte:head>
	<title>{meta.title} · {APP_NAME}</title>
</svelte:head>

{#key `${account}/${key}`}
	<ListPage
		{view}
		path={withAccount(`/label/${encodeURIComponent(key)}`, account)}
		label={meta.label}
		emptyTitle={meta.emptyTitle}
		emptyHint={meta.emptyHint}
		hiddenLabel={key}
	/>
{/key}
