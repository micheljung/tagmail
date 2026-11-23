<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { accounts } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';
	import { accountParam, scopedView, withAccount } from '#lib/views.ts';

	const account = $derived(accountParam(page.url));
	const label = $derived(account === null ? 'Inbox' : `Inbox of ${accounts.name(account)}`);
</script>

<svelte:head>
	<title>{label} · {APP_NAME}</title>
</svelte:head>

{#key account}
	<ListPage
		view={scopedView('inbox', account)}
		path={withAccount('/inbox', account)}
		{label}
		emptyTitle="You're all caught up"
		emptyHint="New conversations land here as soon as they sync."
	/>
{/key}
