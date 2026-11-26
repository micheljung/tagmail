<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { listMeta } from '#lib/listMeta.ts';
	import { listNames, prefs } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';
	import { accountParam, scopedView, withAccount } from '#lib/views.ts';

	const account = $derived(accountParam(page.url));
	const view = $derived(scopedView('inbox', account));
	const meta = $derived(listMeta(view, listNames));
</script>

<svelte:head>
	<title>{meta.title} · {APP_NAME}</title>
</svelte:head>

{#key `${account}|${prefs.inboxType}`}
	<ListPage
		{view}
		path={withAccount('/inbox', account)}
		label={meta.label}
		emptyTitle={meta.emptyTitle}
		emptyHint={meta.emptyHint}
		unreadFirst={prefs.inboxType === 'unread'}
	/>
{/key}
