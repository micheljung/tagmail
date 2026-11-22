<script lang="ts">
	import { APP_NAME } from '#lib/document-title.ts';
	import ListPage from '#lib/ui/ListPage.svelte';
</script>

<svelte:head>
	<title>Inbox · {APP_NAME}</title>
</svelte:head>

<ListPage
	view="inbox"
	path="/inbox"
	label="Inbox"
	emptyTitle="You're all caught up"
	emptyHint="New conversations land here as soon as they sync."
/>
