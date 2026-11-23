<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { accounts } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';
	import { accountParam, scopedView, withAccount } from '#lib/views.ts';

	const query = $derived((page.url.searchParams.get('q') ?? '').trim());
	const account = $derived(accountParam(page.url));
	const scope = $derived(account === null ? '' : ` in ${accounts.name(account)}`);
</script>

<svelte:head>
	<title>{query ? `${query} · ` : ''}Search · {APP_NAME}</title>
</svelte:head>

{#if query}
	{#key `${account}/${query}`}
		<ListPage
			view={scopedView(`search:${query}`, account)}
			path={withAccount(`/search?q=${encodeURIComponent(query)}`, account)}
			label={`Search results for ${query}${scope}`}
			emptyTitle="No conversations match"
			emptyHint="Try fewer words, or operators like from:, label: or after:2026/01/01."
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
