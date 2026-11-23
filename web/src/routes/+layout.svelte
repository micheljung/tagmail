<script lang="ts">
	import '../app.css';
	import { goto } from '$app/navigation';
	import favicon from '#lib/assets/favicon.svg';
	import { composer } from '#lib/compose/composer.svelte.ts';
	import { desktop } from '#lib/desktop/desktop.svelte.ts';
	import { theme } from '#lib/stores/index.ts';
	import { TauriTransport, api } from '#lib/transport/index.ts';
	import { onMount } from 'svelte';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();

	onMount(() => {
		theme.init();
		const transport = api.transport;
		if (!(transport instanceof TauriTransport)) return;
		const stop = desktop.start(api, (event, handler) => transport.listen(event, handler), {
			mailto: (draft) => composer.open(draft),
			pair: () => void goto('/pair')
		});
		return () => void stop.then((unlisten) => unlisten());
	});
</script>

<svelte:head>
	<link rel="icon" href={favicon} />
</svelte:head>

{@render children()}
