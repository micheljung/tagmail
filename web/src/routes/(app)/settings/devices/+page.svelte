<script lang="ts">
	import { APP_NAME } from '#lib/document-title.ts';
	import { fullDate } from '#lib/format.ts';
	import {
		addPasskey,
		api,
		passkeysSupported,
		type DeviceDto,
		type PairingCodeDto,
		type PasskeyDto,
		type SessionDto
	} from '#lib/transport/index.ts';
	import { errorText } from '#lib/ui/errors.ts';
	import Icon from '#lib/ui/Icon.svelte';
	import QrCode from '#lib/ui/QrCode.svelte';
	import { onMount } from 'svelte';

	let sessions = $state<SessionDto[] | null>(null);
	let devices = $state<DeviceDto[] | null>(null);
	let passkeys = $state<PasskeyDto[] | null>(null);
	let canAddPasskey = $state(false);
	let passkeyName = $state('');
	let error = $state<string | null>(null);
	let pairing = $state<PairingCodeDto | null>(null);
	let pairingError = $state<string | null>(null);
	let now = $state(Date.now() / 1000);
	let busy = $state<string | null>(null);

	const remaining = $derived(pairing ? Math.max(0, Math.round(pairing.expiresAt - now)) : 0);

	async function refresh() {
		try {
			[sessions, devices, passkeys] = await Promise.all([
				api.sessions(),
				api.devices(),
				api.passkeys()
			]);
			error = null;
		} catch (e) {
			error = errorText(e);
		}
	}

	onMount(() => {
		canAddPasskey = passkeysSupported();
		passkeyName = defaultPasskeyName();
		void refresh();
		const timer = setInterval(() => (now = Date.now() / 1000), 1000);
		return () => clearInterval(timer);
	});

	async function act(key: string, action: () => Promise<unknown>) {
		busy = key;
		error = null;
		try {
			await action();
			await refresh();
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = null;
		}
	}

	function browserName(ua: string | null): string {
		return ua ? describeAgent(ua) : 'Unknown browser';
	}

	function defaultPasskeyName(): string {
		return describeAgent(navigator.userAgent);
	}

	function describeAgent(ua: string): string {
		const browser = /Firefox/.test(ua)
			? 'Firefox'
			: /Edg\//.test(ua)
				? 'Edge'
				: /Chrome/.test(ua)
					? 'Chrome'
					: /Safari/.test(ua)
						? 'Safari'
						: 'Browser';
		const os = /iPhone|iPad/.test(ua)
			? 'iOS'
			: /Android/.test(ua)
				? 'Android'
				: /Mac OS/.test(ua)
					? 'macOS'
					: /Windows/.test(ua)
						? 'Windows'
						: /Linux/.test(ua)
							? 'Linux'
							: '';
		return os ? `${browser} on ${os}` : browser;
	}

	function submitPasskey(event: SubmitEvent) {
		event.preventDefault();
		void act('passkey', () => addPasskey(api, passkeyName.trim() || defaultPasskeyName()));
	}

	async function pair() {
		pairingError = null;
		busy = 'pair';
		try {
			pairing = await api.startPairing();
			now = Date.now() / 1000;
		} catch (e) {
			pairingError = errorText(e);
		} finally {
			busy = null;
		}
	}

	function seen(ts: number | null): string {
		return ts === null ? 'Never used' : `Last active ${fullDate(ts)}`;
	}

	function clock(s: number): string {
		return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
	}
</script>

<svelte:head>
	<title>Devices &amp; sessions · {APP_NAME}</title>
</svelte:head>

<div class="settings">
	<h1 tabindex="-1">Devices &amp; sessions</h1>
	<p class="lead">Everything signed in to your mail. Revoke anything you don't recognize.</p>

	{#if error}<p class="error" role="alert">{error}</p>{/if}

	<section aria-labelledby="pair-title" class="pair">
		<div class="pair-text">
			<h2 id="pair-title">Pair the desktop app</h2>
			<p>
				In Tagmail for desktop choose <em>Connect to a server</em>, then scan the code or type it. A
				code works once, for five minutes.
			</p>
			{#if !pairing || remaining === 0}
				<button class="btn primary" onclick={pair} disabled={busy === 'pair'}>
					<Icon name="devices" size={18} />{pairing ? 'New pairing code' : 'Pair a device'}
				</button>
			{/if}
			{#if pairingError}<p class="error" role="alert">{pairingError}</p>{/if}
		</div>
		{#if pairing && remaining > 0}
			<div class="code-box">
				<QrCode value={pairing.url} label="Pairing QR code" />
				<div>
					<p class="code" aria-label="Pairing code {pairing.code.split('').join(' ')}">
						{pairing.code.slice(0, 4)}<span class="dash">-</span>{pairing.code.slice(4)}
					</p>
					<p class="expires" aria-live="polite">Expires in {clock(remaining)}</p>
				</div>
			</div>
		{/if}
	</section>

	<section aria-labelledby="passkeys-title">
		<h2 id="passkeys-title">Passkeys</h2>
		{#if passkeys === null}
			<p class="muted">Loading…</p>
		{:else}
			{#if passkeys.length > 0}
				<ul class="items">
					{#each passkeys as k (k.id)}
						<li>
							<span class="kind"><Icon name="shield" size={18} /></span>
							<div class="what">
								<strong>{k.name}</strong>
								<span>Added {fullDate(k.createdAt)}</span>
							</div>
							<button
								class="btn danger"
								onclick={() => act(`passkey-${k.id}`, () => api.removePasskey(k.id))}
								disabled={busy === `passkey-${k.id}`}
							>
								Remove
							</button>
						</li>
					{/each}
				</ul>
			{:else}
				<p class="muted">Sign in with a fingerprint, face or security key instead of a password.</p>
			{/if}
			{#if canAddPasskey}
				<form class="add" onsubmit={submitPasskey}>
					<label>
						<span class="sr-only">Passkey name</span>
						<input bind:value={passkeyName} maxlength="64" placeholder="Name, e.g. Laptop" />
					</label>
					<button class="btn" type="submit" disabled={busy === 'passkey'}>
						<Icon name="shield" size={16} />Add a passkey
					</button>
				</form>
			{/if}
		{/if}
	</section>

	<section aria-labelledby="sessions-title">
		<h2 id="sessions-title">Browser sessions</h2>
		{#if sessions === null}
			<p class="muted">Loading…</p>
		{:else}
			<ul class="items">
				{#each sessions as s (s.id)}
					<li>
						<span class="kind"><Icon name="settings" size={18} /></span>
						<div class="what">
							<strong>{browserName(s.userAgent)}</strong>
							<span>Signed in {fullDate(s.createdAt)} · {seen(s.lastSeenAt)}</span>
						</div>
						{#if s.current}
							<span class="badge">This browser</span>
						{:else}
							<button
								class="btn danger"
								onclick={() => act(`session-${s.id}`, () => api.revokeSession(s.id))}
								disabled={busy === `session-${s.id}`}
							>
								Sign out
							</button>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
	</section>

	<section aria-labelledby="devices-title">
		<h2 id="devices-title">Paired devices</h2>
		{#if devices === null}
			<p class="muted">Loading…</p>
		{:else if devices.length === 0}
			<p class="muted">No desktop app is paired yet.</p>
		{:else}
			<ul class="items">
				{#each devices as d (d.id)}
					<li>
						<span class="kind"><Icon name="devices" size={18} /></span>
						<div class="what">
							<strong>{d.name}</strong>
							<span>Paired {fullDate(d.createdAt)} · {seen(d.lastSeenAt)}</span>
						</div>
						<button
							class="btn danger"
							onclick={() => act(`device-${d.id}`, () => api.revokeDevice(d.id))}
							disabled={busy === `device-${d.id}`}
						>
							Revoke
						</button>
					</li>
				{/each}
			</ul>
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

	.pair {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 24px;
		padding: 22px 24px;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: linear-gradient(135deg, var(--accent-soft), transparent 70%);
	}

	.pair-text {
		flex: 1;
		min-width: 240px;
	}

	.pair-text p {
		margin: 0 0 16px;
		color: var(--ink-2);
	}

	.code-box {
		display: flex;
		align-items: center;
		gap: 20px;
	}

	.code {
		margin: 0;
		font-family: var(--font-mono);
		font-size: 30px;
		font-weight: 600;
		letter-spacing: 0.12em;
	}

	.dash {
		color: var(--faint);
	}

	.expires {
		margin: 4px 0 0;
		color: var(--muted);
		font-family: var(--font-mono);
		font-size: 13px;
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

	.add {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		margin-top: 12px;
	}

	.add input {
		width: min(280px, 100%);
		height: 36px;
		padding: 0 12px;
		border: 1px solid var(--line-strong);
		border-radius: 999px;
		background: var(--surface-2);
	}

	.badge {
		padding: 4px 10px;
		border-radius: 999px;
		background: var(--accent-soft);
		color: var(--accent);
		font-size: 12px;
		font-weight: 650;
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
