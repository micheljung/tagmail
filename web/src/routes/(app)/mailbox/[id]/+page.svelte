<script lang="ts">
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { nav } from '#lib/stores/index.ts';
	import ListPage from '#lib/ui/ListPage.svelte';

	const id = $derived(Number(page.params.id));
	const name = $derived(nav.mailboxName(id));
	const special = $derived(nav.mailboxes.find((m) => m.id === id)?.specialUse ?? null);
	const empty = $derived(
		special === 'trash'
			? 'Trash is empty'
			: special === 'junk'
				? 'No spam here'
				: special === 'drafts'
					? 'No drafts'
					: `${name} is empty`
	);
</script>

<svelte:head>
	<title>{name} · {APP_NAME}</title>
</svelte:head>

{#key id}
	<ListPage
		view={`mailbox:${id}`}
		path={`/mailbox/${id}`}
		label={`Folder ${name}`}
		emptyTitle={empty}
		emptyHint="Conversations in this folder show up here."
	/>
{/key}
