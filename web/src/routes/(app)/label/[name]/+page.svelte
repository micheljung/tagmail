<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { accounts, nav } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';
	import { accountParam, scopedView, withAccount } from '#lib/views.ts';

	const key = $derived(page.params.name ?? '');
	const name = $derived(nav.labelName(key));
	const account = $derived(accountParam(page.url));
	const scope = $derived(account === null ? '' : ` in ${accounts.name(account)}`);
</script>

<svelte:head>
	<title>{name} · {APP_NAME}</title>
</svelte:head>

{#key `${account}/${key}`}
	<ListPage
		view={scopedView(`label:${key}`, account)}
		path={withAccount(`/label/${encodeURIComponent(key)}`, account)}
		label={`Label ${name}${scope}`}
		emptyTitle={`Nothing labeled ${name}`}
		emptyHint="Conversations with this label show up here."
		hiddenLabel={key}
	/>
{/key}
