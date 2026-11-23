<script lang="ts">
	import { APP_NAME } from '#lib/document-title.ts';
	import { accounts, toasts } from '#lib/stores/index.ts';
	import { ApiError, api, type AccountTestDto } from '#lib/transport/index.ts';
	import AccountFields from '#lib/ui/AccountFields.svelte';
	import AccountMarker from '#lib/ui/AccountMarker.svelte';
	import { emptyForm, newAccountOf, type AccountForm } from '#lib/ui/accountForm.ts';
	import { degradedText, phaseText } from '#lib/ui/accountStatus.ts';
	import { errorText } from '#lib/ui/errors.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import { onMount } from 'svelte';

	/** Sync progress shows while the page is open. */
	const STATUS_POLL_MS = 5000;

	let adding = $state(false);
	let form = $state<AccountForm>(emptyForm());
	let errors = $state<Record<string, string>>({});
	let error = $state<string | null>(null);
	let busy = $state<'test' | 'save' | null>(null);
	let test = $state<AccountTestDto | null>(null);
	/** The settings the last passing test used; saving needs a pass for the current ones. */
	let testedWith = $state<string | null>(null);

	const current = $derived(JSON.stringify(newAccountOf(form)));
	const tested = $derived(testedWith === current);

	onMount(() => {
		void accounts.load();
		const timer = setInterval(() => void accounts.load(), STATUS_POLL_MS);
		return () => clearInterval(timer);
	});

	function failed(e: unknown) {
		if (e instanceof ApiError && e.field) errors = { [e.field]: e.message };
		else error = errorText(e);
	}

	async function runTest() {
		busy = 'test';
		errors = {};
		error = null;
		const settings = current;
		try {
			test = await api.testNewAccount(newAccountOf(form));
			testedWith = test.imap.ok && test.smtp.ok ? settings : null;
		} catch (e) {
			test = null;
			failed(e);
		} finally {
			busy = null;
		}
	}

	async function save(event: SubmitEvent) {
		event.preventDefault();
		if (!tested) return void runTest();
		busy = 'save';
		errors = {};
		error = null;
		try {
			const account = await api.createAccount(newAccountOf(form));
			await accounts.load();
			toasts.show(`Added ${account.name}; the first sync is starting`);
			adding = false;
			form = emptyForm();
			test = null;
			testedWith = null;
		} catch (e) {
			failed(e);
		} finally {
			busy = null;
		}
	}
</script>

<svelte:head>
	<title>Accounts · {APP_NAME}</title>
</svelte:head>

<div class="settings">
	<h1 tabindex="-1">Accounts</h1>
	<p class="lead">Every mail account syncs on its own; the inbox and labels show them together.</p>

	{#if error}<p class="error" role="alert">{error}</p>{/if}

	<section aria-labelledby="accounts-title">
		<h2 id="accounts-title">Your accounts</h2>
		{#if !accounts.loaded}
			<p class="muted">Loading…</p>
		{:else if accounts.accounts.length === 0}
			<p class="muted">No accounts yet. Add one below.</p>
		{:else}
			<ul class="items">
				{#each accounts.accounts as account (account.id)}
					{@const status = accounts.statusOf(account.id)}
					<li data-account={account.id}>
						<AccountMarker id={account.id} name={account.name} size={30} />
						<div class="what">
							<strong>{account.name}</strong>
							<span class="muted">{account.email ?? 'No sender address'}</span>
							<span class="phase" class:live={status?.phase === 'live'}>
								{phaseText(status, account.status)}
							</span>
							{#if status && status.degraded.length > 0}
								<ul class="degraded" aria-label="Limitations">
									{#each status.degraded as code (code)}<li>{degradedText(code)}</li>{/each}
								</ul>
							{/if}
						</div>
						<a class="btn" href="/settings/accounts/{account.id}">Edit</a>
					</li>
				{/each}
			</ul>
		{/if}
	</section>

	<section aria-labelledby="add-title">
		<h2 id="add-title">Add an account</h2>
		{#if adding}
			<form class="add" onsubmit={save}>
				<AccountFields bind:form {errors} {test} creating />
				<div class="actions">
					<button class="btn" type="button" onclick={runTest} disabled={busy !== null}>
						<Icon name="sync" size={16} />{busy === 'test' ? 'Testing…' : 'Test connection'}
					</button>
					<button class="btn primary" type="submit" disabled={busy !== null || !tested}>
						<Icon name="plus" size={16} />Add account
					</button>
					<button class="btn" type="button" onclick={() => (adding = false)}>Cancel</button>
					{#if !tested}<span class="hint">Test the connection to add the account.</span>{/if}
				</div>
			</form>
		{:else}
			<button class="btn primary" onclick={() => (adding = true)}>
				<Icon name="plus" size={16} />Add account
			</button>
		{/if}
	</section>
</div>

<style>
	.settings {
		height: 100%;
		padding: 28px 36px 48px;
		overflow-y: auto;
	}

	h1 {
		margin: 0;
		font-size: 24px;
		font-weight: 600;
		letter-spacing: -0.015em;
		outline: none;
	}

	.lead {
		margin: 6px 0 26px;
		color: var(--muted);
	}

	section {
		max-width: 820px;
		margin-bottom: 30px;
	}

	h2 {
		margin: 0 0 10px;
		font-size: 15px;
		font-weight: 650;
	}

	.items {
		margin: 0;
		padding: 0;
		border: 1px solid var(--line);
		border-radius: var(--radius);
		list-style: none;
	}

	.items > li {
		display: flex;
		align-items: center;
		gap: 14px;
		padding: 14px 16px;
		border-bottom: 1px solid var(--line);
	}

	.items > li:last-child {
		border-bottom: 0;
	}

	.what {
		display: grid;
		flex: 1;
		gap: 2px;
		min-width: 0;
	}

	.what strong {
		font-weight: 600;
	}

	.phase {
		color: var(--ink-2);
		font-size: 12.5px;
	}

	.phase.live {
		color: var(--ok);
	}

	.degraded {
		margin: 4px 0 0;
		padding-left: 18px;
		color: var(--danger);
		font-size: 12.5px;
	}

	.add {
		display: grid;
		gap: 16px;
	}

	.actions {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
	}

	.hint,
	.muted {
		color: var(--muted);
		font-size: 13px;
	}

	.error {
		max-width: 820px;
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
	}

	@media (max-width: 720px) {
		.settings {
			padding: 20px 16px 40px;
		}
	}
</style>
