<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { listMeta } from '#lib/listMeta.ts';
	import { listNames } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';
	import { accountParam, scopedView, withAccount } from '#lib/views.ts';

	const query = $derived((page.url.searchParams.get('q') ?? '').trim());
	const account = $derived(accountParam(page.url));
	const view = $derived(scopedView(`search:${query}`, account));
	const meta = $derived(listMeta(view, listNames));
</script>

<svelte:head>
	<title>{meta.title} · {APP_NAME}</title>
</svelte:head>

{#if query}
	{#key `${account}/${query}`}
		<ListPage
			{view}
			path={withAccount(`/search?q=${encodeURIComponent(query)}`, account)}
			label={meta.label}
			emptyTitle={meta.emptyTitle}
			emptyHint={meta.emptyHint}
		/>
	{/key}
{:else}
	<div class="blank">
		<strong>Search your mail</strong>
		<span>Press <kbd>/</kbd> and type words, a phrase in quotes, or an operator.</span>
	</div>
{/if}

<style>
	.blank {
		display: grid;
		place-content: center;
		gap: 8px;
		height: 100%;
		text-align: center;
		color: var(--muted);
	}

	strong {
		color: var(--ink);
		font-size: 16px;
	}
</style>
