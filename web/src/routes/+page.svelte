<script lang="ts">
	import { documentTitle } from '#lib';
</script>

<svelte:head>
	<title>{documentTitle(0)}</title>
</svelte:head>

<header>
	<h1>Tagmail</h1>
</header>
<main></main>

<style>
	header {
		padding: 0.75rem 1rem;
		border-bottom: 1px solid light-dark(#ddd, #333);
	}

	h1 {
		margin: 0;
		font-size: 1.25rem;
	}
</style>
