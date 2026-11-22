<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { api } from '#lib/transport/index.ts';
	import AuthCard from '#lib/ui/AuthCard.svelte';
	import { errorText } from '#lib/ui/errors.ts';

	let token = $state(page.url.searchParams.get('token') ?? '');
	let password = $state('');
	let confirm = $state('');
	let busy = $state(false);
	let error = $state<string | null>(null);

	const mismatch = $derived(confirm.length > 0 && confirm !== password);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (password !== confirm) return;
		busy = true;
		error = null;
		try {
			await api.setup(token.trim(), password);
			await goto('/inbox', { replaceState: true });
		} catch (e) {
			error = errorText(e);
			busy = false;
		}
	}
</script>

<svelte:head>
	<title>Set up · {APP_NAME}</title>
</svelte:head>

<AuthCard
	title="Set up Tagmail"
	lead="Paste the one-time setup token from the server log, then choose the owner password."
>
	<form onsubmit={submit}>
		{#if error}<p class="error" role="alert">{error}</p>{/if}
		<label>
			Setup token
			<input name="token" autocomplete="off" spellcheck="false" required bind:value={token} />
		</label>
		<label>
			Password
			<input
				type="password"
				name="new-password"
				autocomplete="new-password"
				required
				bind:value={password}
			/>
		</label>
		<label>
			Repeat password
			<input
				type="password"
				name="confirm"
				autocomplete="new-password"
				required
				aria-invalid={mismatch}
				aria-describedby={mismatch ? 'mismatch' : undefined}
				bind:value={confirm}
			/>
		</label>
		{#if mismatch}<p class="error" id="mismatch">The passwords don't match.</p>{/if}
		<button class="btn primary" type="submit" disabled={busy || !token || !password || mismatch}>
			{busy ? 'Setting up…' : 'Create owner login'}
		</button>
	</form>
	<p class="alt">Already set up? <a href="/login">Sign in</a></p>
</AuthCard>
