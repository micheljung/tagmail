<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { APP_NAME } from '#lib/document-title.ts';
	import { ApiError, api, passkeysSupported, signInWithPasskey } from '#lib/transport/index.ts';
	import AuthCard from '#lib/ui/AuthCard.svelte';
	import { errorText } from '#lib/ui/errors.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import { onMount } from 'svelte';

	let password = $state('');
	let busy = $state<'password' | 'passkey' | null>(null);
	let error = $state<string | null>(null);
	let passkeys = $state(false);

	onMount(() => {
		passkeys = passkeysSupported();
		api.session().then(
			() => goto(next(), { replace: true }),
			(e: unknown) => {
				if (e instanceof ApiError && e.code === 'setup_required') void goto('/setup');
			}
		);
	});

	function next(): string {
		const n = page.url.searchParams.get('next');
		return n && n.startsWith('/') && !n.startsWith('//') ? n : '/inbox';
	}

	async function run(kind: 'password' | 'passkey', action: () => Promise<unknown>) {
		busy = kind;
		error = null;
		try {
			await action();
			await goto(next(), { replace: true });
		} catch (e) {
			error = errorText(e);
			busy = null;
		}
	}

	function submit(event: SubmitEvent) {
		event.preventDefault();
		void run('password', () => api.login(password));
	}
</script>

<svelte:head>
	<title>Sign in · {APP_NAME}</title>
</svelte:head>

<AuthCard title="Welcome back" lead="Sign in to your mail.">
	{#if passkeys}
		<button
			class="btn passkey"
			type="button"
			disabled={busy !== null}
			onclick={() => run('passkey', () => signInWithPasskey(api))}
		>
			<Icon name="shield" size={18} />
			{busy === 'passkey' ? 'Waiting for your passkey…' : 'Sign in with a passkey'}
		</button>
		<p class="or"><span>or use your password</span></p>
	{/if}
	<form onsubmit={submit}>
		{#if error}<p class="error" role="alert">{error}</p>{/if}
		<label>
			Password
			<input
				type="password"
				name="password"
				autocomplete="current-password"
				required
				bind:value={password}
			/>
		</label>
		<button class="btn primary" type="submit" disabled={busy !== null || !password}>
			{busy === 'password' ? 'Signing in…' : 'Sign in'}
		</button>
	</form>
	<p class="alt">First time here? <a href="/setup">Set up this server</a></p>
</AuthCard>

<style>
	.passkey {
		justify-content: center;
		width: 100%;
		height: 44px;
	}

	.or {
		display: flex;
		align-items: center;
		gap: 10px;
		margin: 18px 0;
		color: var(--muted);
		font-size: 12px;
	}

	.or::before,
	.or::after {
		content: '';
		flex: 1;
		height: 1px;
		background: var(--line);
	}
</style>
