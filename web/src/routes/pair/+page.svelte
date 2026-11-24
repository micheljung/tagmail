<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { desktop } from '#lib/desktop/desktop.svelte.ts';
	import { APP_NAME } from '#lib/document-title.ts';
	import { api } from '#lib/transport/index.ts';
	import AuthCard from '#lib/ui/AuthCard.svelte';
	import { errorText } from '#lib/ui/errors.ts';

	let server = $state(page.url.searchParams.get('server') ?? '');
	let code = $state(page.url.searchParams.get('code') ?? '');
	let busy = $state(false);
	let error = $state<string | null>(null);

	$effect(() => {
		if (!desktop.pairing) return;
		server = desktop.pairing.server;
		code = desktop.pairing.code;
		desktop.pairing = null;
	});

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		busy = true;
		error = null;
		try {
			desktop.setStatus(await api.pair({ server: server.trim(), code: code.trim() }));
			await goto('/inbox', { replaceState: true });
		} catch (e) {
			error = errorText(e);
			busy = false;
		}
	}
</script>

<svelte:head>
	<title>Connect to a server · {APP_NAME}</title>
</svelte:head>

<AuthCard
	title="Connect to a server"
	lead="On the web, open Devices & sessions and choose Pair a device, then enter the code here."
>
	<form onsubmit={submit}>
		{#if error}<p class="error" role="alert">{error}</p>{/if}
		{#each desktop.status?.warnings ?? [] as warning (warning)}
			<p class="warning" role="status" data-desktop="warning">{warning}</p>
		{/each}
		<label>
			Server address
			<input
				type="url"
				name="server"
				placeholder="https://mail.example.com"
				autocomplete="url"
				required
				bind:value={server}
			/>
		</label>
		<label>
			Pairing code
			<input
				name="code"
				autocomplete="one-time-code"
				autocapitalize="characters"
				spellcheck="false"
				required
				bind:value={code}
			/>
		</label>
		<button class="btn primary" type="submit" disabled={busy || !server || !code}>
			{busy ? 'Connecting…' : 'Connect'}
		</button>
	</form>
</AuthCard>
