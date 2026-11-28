<script lang="ts">
	import { goto } from '$app/navigation';
	import { desktop } from '#lib/desktop/desktop.svelte.ts';
	import { APP_NAME } from '#lib/document-title.ts';
	import { fullDate } from '#lib/format.ts';
	import { api, type DesktopSettingsDto } from '#lib/transport/index.ts';
	import { errorText } from '#lib/ui/errors.ts';
	import { onMount } from 'svelte';

	const WINDOW_CHOICES = [30, 90, 180, 365];

	let settings = $state<DesktopSettingsDto | null>(null);
	let error = $state<string | null>(null);
	let busy = $state(false);

	onMount(async () => {
		try {
			[settings] = await Promise.all([
				api.desktopSettings(),
				api.desktopStatus().then((s) => desktop.setStatus(s))
			]);
		} catch (e) {
			error = errorText(e);
		}
	});

	async function update(patch: Partial<DesktopSettingsDto>) {
		error = null;
		try {
			settings = await api.updateDesktopSettings(patch);
		} catch (e) {
			error = errorText(e);
		}
	}

	async function unpair() {
		busy = true;
		try {
			await api.unpair();
			await goto('/pair');
		} catch (e) {
			error = errorText(e);
			busy = false;
		}
	}
</script>

<svelte:head>
	<title>This device · {APP_NAME}</title>
</svelte:head>

<div class="settings">
	<h1 tabindex="-1">This device</h1>
	<p class="lead">
		Mail kept on this computer, notifications and startup. Passkeys, sessions and other devices are
		managed on the web.
	</p>

	{#if error}<p class="error" role="alert">{error}</p>{/if}

	<section aria-labelledby="server-title">
		<h2 id="server-title">Server</h2>
		{#if desktop.status}
			<dl>
				<dt>Connected to</dt>
				<dd>{desktop.status.server ?? 'Not paired'}</dd>
				<dt>Status</dt>
				<dd>{desktop.status.online ? 'Online' : 'Offline'}</dd>
				<dt>Last sync</dt>
				<dd>{desktop.status.lastSync ? fullDate(desktop.status.lastSync) : 'Never'}</dd>
			</dl>
			<button class="btn danger" onclick={unpair} disabled={busy}>Disconnect this device</button>
		{:else}
			<p class="muted">Loading…</p>
		{/if}
	</section>

	{#if settings}
		<section aria-labelledby="offline-title">
			<h2 id="offline-title">Offline mail</h2>
			<label>
				<span>Keep searchable on this device</span>
				<select
					value={settings.windowDays}
					onchange={(e) => update({ windowDays: Number(e.currentTarget.value) })}
				>
					{#each WINDOW_CHOICES as days (days)}
						<option value={days}>Last {days} days</option>
					{/each}
				</select>
			</label>
			<label>
				<span>Disk space for full messages (MB)</span>
				<input
					type="number"
					min="0"
					step="256"
					value={settings.rawCapMb}
					onchange={(e) => update({ rawCapMb: Number(e.currentTarget.value) })}
				/>
			</label>
		</section>

		<section aria-labelledby="app-title">
			<h2 id="app-title">App</h2>
			<label class="check">
				<input
					type="checkbox"
					checked={settings.notify}
					onchange={(e) => update({ notify: e.currentTarget.checked })}
				/>
				<span>Notify me about new mail</span>
			</label>
			<label class="check">
				<input
					type="checkbox"
					checked={settings.notifyContent}
					disabled={!settings.notify}
					onchange={(e) => update({ notifyContent: e.currentTarget.checked })}
				/>
				<span>Show sender and subject in notifications</span>
			</label>
			<label class="check">
				<input
					type="checkbox"
					checked={settings.reminders}
					disabled={!settings.notify}
					onchange={(e) => update({ reminders: e.currentTarget.checked })}
				/>
				<span>Calendar reminders</span>
			</label>
			<label class="check">
				<input
					type="checkbox"
					checked={settings.autostart}
					onchange={(e) => update({ autostart: e.currentTarget.checked })}
				/>
				<span>Start Tagmail when I sign in</span>
			</label>
			<label class="check">
				<input
					type="checkbox"
					checked={settings.closeToTray}
					onchange={(e) => update({ closeToTray: e.currentTarget.checked })}
				/>
				<span>Keep running in the tray when the window closes</span>
			</label>
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
		margin: 6px 0 26px;
		color: var(--muted);
	}

	section {
		display: grid;
		gap: 12px;
		max-width: 560px;
		margin-bottom: 30px;
	}

	h2 {
		margin: 0;
		font-size: 15px;
		font-weight: 650;
	}

	dl {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: 6px 16px;
		margin: 0;
	}

	dt {
		color: var(--muted);
	}

	dd {
		margin: 0;
	}

	label {
		display: grid;
		gap: 6px;
	}

	label.check {
		display: flex;
		align-items: center;
		gap: 10px;
	}

	.btn {
		justify-self: start;
	}

	.error {
		color: var(--danger);
	}

	.muted {
		color: var(--muted);
	}
</style>
