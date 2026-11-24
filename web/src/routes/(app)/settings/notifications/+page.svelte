<script lang="ts">
	import type { PushSubscriptionDto } from '@tagmail/api-types';
	import { APP_NAME } from '#lib/document-title.ts';
	import { describeAgent, fullDate } from '#lib/format.ts';
	import {
		disablePush,
		enablePush,
		pushAvailability,
		pushEnv,
		syncPush,
		updatePush,
		type PushAvailability
	} from '#lib/push/client.ts';
	import { pushPrefs } from '#lib/push/prefs.ts';
	import { accounts } from '#lib/stores/index.ts';
	import { api, isDesktop } from '#lib/transport/index.ts';
	import { errorText } from '#lib/ui/errors.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import { onMount } from 'svelte';

	let availability = $state<PushAvailability | null>(null);
	let manager: PushManager | null = null;
	let mine = $state<PushSubscriptionDto | null>(null);
	let others = $state<PushSubscriptionDto[]>([]);
	let busy = $state<string | null>(null);
	let error = $state<string | null>(null);

	const accountIds = $derived(accounts.accounts.map((a) => a.id));

	async function refresh() {
		const all = await api.pushSubscriptions();
		others = all.filter((s) => !s.current);
		if (mine) mine = all.find((s) => s.id === mine?.id) ?? null;
	}

	async function start() {
		if (isDesktop()) {
			availability = 'unsupported';
			return;
		}
		availability = pushAvailability(pushEnv());
		void accounts.load();
		const registration = await navigator.serviceWorker?.getRegistration();
		manager = registration?.pushManager ?? null;
		if (availability === 'ready' && !manager) availability = 'unsupported';
		if (manager && Notification.permission === 'granted')
			mine = await syncPush(manager, api, pushPrefs);
		await refresh();
	}

	onMount(() => {
		start().catch((e: unknown) => (error = errorText(e)));
	});

	async function act(key: string, action: () => Promise<unknown>) {
		busy = key;
		error = null;
		try {
			await action();
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = null;
		}
	}

	function enable() {
		const push = manager;
		if (!push) return;
		void act('enable', async () => {
			mine = await enablePush(push, api, pushPrefs, () => Notification.requestPermission());
			if (!mine) availability = pushAvailability(pushEnv());
		});
	}

	function turnOff() {
		const push = manager;
		if (!push) return;
		void act('off', async () => {
			await disablePush(push, api, pushPrefs, mine);
			mine = null;
		});
	}

	function setPreview(preview: boolean) {
		const row = mine;
		if (!row) return;
		void act('preview', async () => {
			mine = await updatePush(api, pushPrefs, row.id, { preview });
		});
	}

	function notifies(row: PushSubscriptionDto, id: number): boolean {
		return row.accountIds === null || row.accountIds.includes(id);
	}

	function setAccount(id: number, on: boolean) {
		const row = mine;
		if (!row) return;
		const chosen = accountIds.filter((a) => (a === id ? on : notifies(row, a)));
		const all = chosen.length === accountIds.length;
		void act('accounts', async () => {
			mine = await updatePush(api, pushPrefs, row.id, {
				accountIds: all ? null : chosen
			});
		});
	}

	function remove(row: PushSubscriptionDto) {
		void act(`remove-${row.id}`, async () => {
			await api.removePushSubscription(row.id);
			await refresh();
		});
	}

	function deviceName(row: PushSubscriptionDto): string {
		return row.userAgent ? describeAgent(row.userAgent) : 'Unknown browser';
	}
</script>

<svelte:head>
	<title>Notifications · {APP_NAME}</title>
</svelte:head>

<div class="settings">
	<h1 tabindex="-1">Notifications</h1>
	<p class="lead">
		Get notified about new mail in your inbox. Notifications stop when this browser's session ends;
		opening Tagmail keeps it going.
	</p>

	{#if error}<p class="error" role="alert">{error}</p>{/if}

	<section aria-labelledby="device-title">
		<h2 id="device-title">This device</h2>
		{#if availability === null}
			<p class="muted">Loading…</p>
		{:else if availability === 'unsupported'}
			<p class="muted" data-push-state="unsupported">Not supported in this browser</p>
		{:else if availability === 'ios-tab'}
			<p class="muted" data-push-state="ios-tab">
				On iPhone and iPad, add Tagmail to your Home Screen first: Share → Add to Home Screen, then
				open it from there.
			</p>
		{:else if availability === 'blocked'}
			<p class="muted" data-push-state="blocked">Blocked in browser settings</p>
		{:else if !mine}
			<button class="btn primary" onclick={enable} disabled={busy === 'enable'}>
				<Icon name="bell" size={18} />Enable notifications
			</button>
		{:else}
			<div class="enabled" data-push-state="enabled">
				<label class="check">
					<input
						type="checkbox"
						checked={mine.preview}
						disabled={busy === 'preview'}
						onchange={(e) => setPreview(e.currentTarget.checked)}
					/>
					<span>Show sender and subject</span>
				</label>
				{#if accounts.multi}
					<fieldset>
						<legend>Accounts</legend>
						{#each accounts.accounts as account (account.id)}
							{@const on = notifies(mine, account.id)}
							<label class="check">
								<input
									type="checkbox"
									checked={on}
									disabled={busy === 'accounts' ||
										(on && (mine.accountIds?.length ?? accountIds.length) === 1)}
									onchange={(e) => setAccount(account.id, e.currentTarget.checked)}
								/>
								<span>{account.name}</span>
							</label>
						{/each}
					</fieldset>
				{/if}
				<div class="actions">
					<button
						class="btn"
						onclick={() => {
							const row = mine;
							if (row) void act('test', () => api.testPush(row.id));
						}}
						disabled={busy === 'test'}
					>
						Send test notification
					</button>
					<button class="btn danger" onclick={turnOff} disabled={busy === 'off'}>
						Turn off on this device
					</button>
				</div>
			</div>
		{/if}
	</section>

	{#if others.length > 0}
		<section aria-labelledby="others-title">
			<h2 id="others-title">Other devices</h2>
			<ul class="items">
				{#each others as row (row.id)}
					<li data-push-device={row.id}>
						<span class="kind"><Icon name="devices" size={18} /></span>
						<div class="what">
							<strong>{deviceName(row)}</strong>
							<span>
								{row.endpointHost} · {row.lastSuccessAt === null
									? 'No notification yet'
									: `Last notified ${fullDate(row.lastSuccessAt)}`}
							</span>
						</div>
						<button
							class="btn danger"
							onclick={() => remove(row)}
							disabled={busy === `remove-${row.id}`}
						>
							Remove
						</button>
					</li>
				{/each}
			</ul>
		</section>
	{/if}
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
		max-width: 820px;
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

	.enabled {
		display: grid;
		gap: 14px;
	}

	label.check {
		display: flex;
		align-items: center;
		gap: 10px;
	}

	fieldset {
		display: grid;
		gap: 8px;
		margin: 0;
		padding: 0;
		border: 0;
	}

	legend {
		margin-bottom: 6px;
		color: var(--muted);
		font-size: 13px;
	}

	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
	}

	.items {
		margin: 0;
		padding: 0;
		border: 1px solid var(--line);
		border-radius: var(--radius);
		list-style: none;
	}

	.items li {
		display: flex;
		align-items: center;
		gap: 14px;
		padding: 14px 16px;
		border-bottom: 1px solid var(--line);
	}

	.items li:last-child {
		border-bottom: 0;
	}

	.kind {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border-radius: 10px;
		background: var(--surface-2);
		color: var(--muted);
	}

	.what {
		display: grid;
		flex: 1;
		min-width: 0;
		gap: 2px;
	}

	.what strong {
		overflow: hidden;
		font-weight: 600;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.what span {
		color: var(--muted);
		font-size: 12.5px;
	}

	.muted {
		color: var(--muted);
	}

	.error {
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
	}

	@media (max-width: 720px) {
		.settings {
			padding: 20px 16px 40px;
		}

		.items li {
			flex-wrap: wrap;
		}
	}
</style>
