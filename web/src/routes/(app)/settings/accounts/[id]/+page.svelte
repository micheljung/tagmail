<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { composer } from '#lib/compose/composer.svelte.ts';
	import { APP_NAME } from '#lib/document-title.ts';
	import { accounts, toasts } from '#lib/stores/index.ts';
	import {
		ApiError,
		api,
		type AccountDto,
		type AccountTestDto,
		type IdentityDto
	} from '#lib/transport/index.ts';
	import AccountFields from '#lib/ui/AccountFields.svelte';
	import AccountMarker from '#lib/ui/AccountMarker.svelte';
	import { formOf, patchOf, type AccountForm } from '#lib/ui/accountForm.ts';
	import { degradedText, phaseText } from '#lib/ui/accountStatus.ts';
	import { errorText } from '#lib/ui/errors.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import IdentityEditor, { type IdentityInput } from '#lib/ui/IdentityEditor.svelte';
	import { onMount } from 'svelte';

	const id = $derived(Number(page.params.id));
	const account = $derived(accounts.get(id));
	const status = $derived(accounts.statusOf(id));
	const identities = $derived(composer.identities.filter((i) => i.accountId === id));

	let form = $state<AccountForm | null>(null);
	/** The account `form` was filled from; edits survive status reloads. */
	let formFor = $state<number | null>(null);
	let errors = $state<Record<string, string>>({});
	let error = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let test = $state<AccountTestDto | null>(null);
	/** Identity being edited: its id, `'new'`, or nothing. */
	let editing = $state<number | 'new' | null>(null);
	let identityError = $state<string | null>(null);
	let confirmName = $state('');
	let confirming = $state(false);

	$effect(() => {
		if (!account || formFor === account.id) return;
		form = formOf(account);
		formFor = account.id;
		test = null;
	});

	onMount(() => {
		void accounts.load();
		void composer.reloadIdentities().catch(() => undefined);
	});

	async function step(key: string, run: () => Promise<unknown>): Promise<boolean> {
		busy = key;
		error = null;
		errors = {};
		try {
			await run();
			return true;
		} catch (e) {
			if (e instanceof ApiError && e.field) errors = { [e.field]: e.message };
			else error = errorText(e);
			return false;
		} finally {
			busy = null;
		}
	}

	function runTest(a: AccountDto, f: AccountForm) {
		void step('test', async () => {
			test = await api.testAccount(a.id, patchOf(f, a));
		});
	}

	async function save(event: SubmitEvent, a: AccountDto, f: AccountForm) {
		event.preventDefault();
		const ok = await step('save', () => api.updateAccount(a.id, patchOf(f, a)));
		if (!ok) return;
		await accounts.load();
		const fresh = accounts.get(a.id);
		if (fresh) form = formOf(fresh);
		toasts.show(`Saved ${a.name}`);
	}

	async function setEnabled(a: AccountDto, on: boolean) {
		if (
			await step('status', () => api.updateAccount(a.id, { status: on ? 'active' : 'disabled' }))
		) {
			await accounts.load();
		}
	}

	async function remove(a: AccountDto) {
		if (confirmName !== a.name) return;
		if (await step('delete', () => api.deleteAccount(a.id))) {
			toasts.show(`Deleting ${a.name} and its cached mail`);
			await accounts.load();
			await goto('/settings/accounts');
		}
	}

	async function saveIdentity(input: IdentityInput, identity: IdentityDto | null) {
		busy = 'identity';
		identityError = null;
		try {
			if (identity) {
				await api.updateIdentity(identity.id, {
					address: input.address,
					name: input.name,
					signature: input.signature
				});
			} else {
				await api.createIdentity({
					accountId: id,
					address: input.address,
					...(input.name ? { name: input.name } : {}),
					...(input.signature ? { signature: input.signature } : {})
				});
			}
			await composer.reloadIdentities();
			editing = null;
		} catch (e) {
			identityError = errorText(e);
		} finally {
			busy = null;
		}
	}

	async function identityAction(key: string, run: () => Promise<unknown>) {
		if (await step(key, run)) await composer.reloadIdentities().catch(() => undefined);
	}
</script>

<svelte:head>
	<title>{account?.name ?? 'Account'} · {APP_NAME}</title>
</svelte:head>

<div class="settings">
	<a class="back" href="/settings/accounts"><Icon name="back" size={16} />Accounts</a>
	{#if !account}
		<h1 tabindex="-1">Account</h1>
		<p class="muted">{accounts.loaded ? 'This account no longer exists.' : 'Loading…'}</p>
	{:else}
		<header>
			<AccountMarker id={account.id} name={account.name} size={36} />
			<div>
				<h1 tabindex="-1">{account.name}</h1>
				<p class="lead">
					{account.email ?? 'No sender address'} · {phaseText(status, account.status)}
				</p>
			</div>
		</header>

		{#if error}<p class="error" role="alert">{error}</p>{/if}
		{#if status && status.degraded.length > 0}
			<ul class="degraded" aria-label="Limitations">
				{#each status.degraded as code (code)}<li>{degradedText(code)}</li>{/each}
			</ul>
		{/if}

		<section aria-labelledby="identities-title">
			<h2 id="identities-title">Send mail as</h2>
			<ul class="items">
				{#each identities as identity (identity.id)}
					<li data-identity={identity.id}>
						{#if editing === identity.id}
							<IdentityEditor
								{identity}
								busy={busy === 'identity'}
								error={identityError}
								onsave={(input) => saveIdentity(input, identity)}
								oncancel={() => (editing = null)}
							/>
						{:else}
							<div class="line">
								<div class="what">
									<strong>
										{identity.name ? `${identity.name} <${identity.address}>` : identity.address}
									</strong>
									<span class="muted">
										{identity.signature ? 'Has a signature' : 'No signature'}
									</span>
								</div>
								{#if identity.isDefault}
									<span class="badge">Default</span>
								{:else}
									<button
										class="btn"
										disabled={busy !== null}
										onclick={() =>
											identityAction(`default-${identity.id}`, () =>
												api.updateIdentity(identity.id, { isDefault: true })
											)}
									>
										Make default
									</button>
								{/if}
								<button
									class="btn"
									onclick={() => {
										identityError = null;
										editing = identity.id;
									}}
									aria-label="Edit {identity.address}"
								>
									Edit
								</button>
								<button
									class="btn danger"
									disabled={busy !== null || identities.length === 1}
									title={identities.length === 1 ? 'An account needs one identity' : undefined}
									onclick={() =>
										identityAction(`delete-${identity.id}`, () => api.deleteIdentity(identity.id))}
									aria-label="Delete {identity.address}"
								>
									Delete
								</button>
							</div>
						{/if}
					</li>
				{/each}
				{#if editing === 'new'}
					<li>
						<IdentityEditor
							busy={busy === 'identity'}
							error={identityError}
							onsave={(input) => saveIdentity(input, null)}
							oncancel={() => (editing = null)}
						/>
					</li>
				{/if}
			</ul>
			{#if editing !== 'new'}
				<button
					class="btn add-identity"
					onclick={() => {
						identityError = null;
						editing = 'new';
					}}
				>
					<Icon name="plus" size={16} />Add address
				</button>
			{/if}
		</section>

		{#if form}
			<section aria-labelledby="connection-title">
				<h2 id="connection-title">Connection</h2>
				<form class="connection" onsubmit={(e) => save(e, account, form!)}>
					<AccountFields bind:form {errors} {test} />
					<div class="actions">
						<button
							class="btn"
							type="button"
							onclick={() => runTest(account, form!)}
							disabled={busy !== null}
						>
							<Icon name="sync" size={16} />{busy === 'test' ? 'Testing…' : 'Test connection'}
						</button>
						<button class="btn primary" type="submit" disabled={busy !== null}>Save</button>
					</div>
				</form>
			</section>
		{/if}

		<section aria-labelledby="status-title">
			<h2 id="status-title">Sync</h2>
			{#if account.status === 'disabled'}
				<p class="muted">Disabled: nothing syncs or sends until you turn it back on.</p>
				<button
					class="btn primary"
					disabled={busy !== null}
					onclick={() => setEnabled(account, true)}
				>
					Enable account
				</button>
			{:else}
				<p class="muted">Stops syncing and sending; cached mail stays.</p>
				<button class="btn" disabled={busy !== null} onclick={() => setEnabled(account, false)}>
					Disable account
				</button>
			{/if}
		</section>

		<section aria-labelledby="delete-title" class="danger-zone">
			<h2 id="delete-title">Delete account</h2>
			<p class="muted">
				Removes the account, its identities and every cached message from Tagmail. Mail on the
				server is not touched.
			</p>
			{#if confirming}
				<label class="confirm">
					<span>Type <strong>{account.name}</strong> to confirm</span>
					<input bind:value={confirmName} autocomplete="off" aria-label="Account name to confirm" />
				</label>
				<div class="actions">
					<button
						class="btn danger"
						disabled={confirmName !== account.name || busy !== null}
						onclick={() => remove(account)}
					>
						Delete {account.name}
					</button>
					<button class="btn" onclick={() => (confirming = false)}>Keep</button>
				</div>
			{:else}
				<button class="btn danger" onclick={() => (confirming = true)}>Delete account…</button>
			{/if}
		</section>
	{/if}
</div>

<style>
	.settings {
		height: 100%;
		padding: 20px 36px 48px;
		overflow-y: auto;
	}

	.back {
		display: inline-flex;
		align-items: center;
		gap: 6px;
		margin-bottom: 14px;
		color: var(--muted);
		font-size: 13px;
		text-decoration: none;
	}

	header {
		display: flex;
		align-items: center;
		gap: 14px;
		margin-bottom: 22px;
	}

	h1 {
		margin: 0;
		font-size: 24px;
		font-weight: 600;
		letter-spacing: -0.015em;
		outline: none;
	}

	.lead {
		margin: 2px 0 0;
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
		overflow: hidden;
		border: 1px solid var(--line);
		border-radius: var(--radius);
		list-style: none;
	}

	.items > li {
		border-bottom: 1px solid var(--line);
	}

	.items > li:last-child {
		border-bottom: 0;
	}

	.line {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 12px 16px;
	}

	.what {
		display: grid;
		flex: 1;
		gap: 2px;
		min-width: 0;
	}

	.what strong {
		overflow: hidden;
		font-weight: 600;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.badge {
		padding: 3px 10px;
		border-radius: 999px;
		background: var(--accent-soft);
		color: var(--accent);
		font-size: 12px;
		font-weight: 650;
	}

	.add-identity {
		margin-top: 10px;
	}

	.connection {
		display: grid;
		gap: 16px;
	}

	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
	}

	.degraded {
		max-width: 820px;
		margin: 0 0 22px;
		padding: 10px 12px 10px 30px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
		font-size: 13px;
	}

	.danger-zone {
		padding-top: 20px;
		border-top: 1px solid var(--line);
	}

	.confirm {
		display: grid;
		gap: 6px;
		max-width: 320px;
		margin-bottom: 10px;
		font-size: 13px;
	}

	.confirm input {
		height: 36px;
		padding: 0 12px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface-2);
		color: var(--ink);
	}

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
			padding: 16px 16px 40px;
		}

		.line {
			flex-wrap: wrap;
		}
	}
</style>
