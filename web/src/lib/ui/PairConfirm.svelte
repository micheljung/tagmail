<script lang="ts">
	import { desktop } from '../desktop/desktop.svelte';
	import { serverHost, serverIdentity } from '../desktop/host';

	let { onconfirm }: { onconfirm: () => void } = $props();

	let dialog: HTMLDialogElement;

	const currentServer = $derived(desktop.status?.server ?? null);
	const nextServer = $derived(desktop.repair?.server ?? '');
	const other = $derived(
		!currentServer || serverIdentity(currentServer) !== serverIdentity(nextServer)
	);
	const sameHost = $derived(
		!!currentServer && serverHost(currentServer) === serverHost(nextServer)
	);
	const label = (server: string) => (sameHost ? serverIdentity(server) : serverHost(server));
	const current = $derived(currentServer ? label(currentServer) : null);
	const next = $derived(label(nextServer));

	$effect(() => {
		if (desktop.repair && !dialog.open) dialog.showModal();
		else if (!desktop.repair && dialog.open) dialog.close();
	});
</script>

<dialog
	bind:this={dialog}
	aria-labelledby="pair-confirm-title"
	data-desktop="pair-confirm"
	onclose={() => (desktop.repair = null)}
>
	<h2 id="pair-confirm-title">Connect to a different server?</h2>
	<p>
		A pairing link asks to connect this device to <strong data-host="new">{next}</strong>
		instead of
		<strong data-host="current">{current ?? 'your current server'}</strong>.
	</p>
	{#if other}
		<p>
			Changes not yet sent to {current ?? 'your current server'} stay on this device and are never sent
			to {next}.
		</p>
	{/if}
	<p>Only continue if you opened this link yourself.</p>
	<div class="actions">
		<button class="btn" onclick={() => (desktop.repair = null)}>Cancel</button>
		<button class="btn primary" onclick={onconfirm}>Continue</button>
	</div>
</dialog>

<style>
	dialog {
		width: min(460px, calc(100vw - 32px));
		padding: 20px 24px;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	dialog::backdrop {
		background: rgb(8 10 14 / 0.45);
	}

	h2 {
		margin: 0 0 8px;
		font-size: 18px;
		font-weight: 650;
	}

	strong {
		overflow-wrap: anywhere;
	}

	.actions {
		display: flex;
		justify-content: flex-end;
		gap: 8px;
		margin-top: 16px;
	}
</style>
