<script lang="ts">
	import type {
		CaldavDiscoveryDto,
		CaldavSettingsDto,
		CaldavSettingsInputDto,
		CaldavStatusDto,
		SchedulingOverrideDto
	} from '@tagmail/api-types';
	import { accounts, calendars, toasts } from '#lib/stores/index.ts';
	import { api } from '#lib/transport/index.ts';
	import { errorText } from './errors';
	import Icon from './Icon.svelte';

	let { accountId }: { accountId: number } = $props();

	const STATUS_TEXT: Record<CaldavStatusDto, string> = {
		unconfigured: 'Not set up',
		needs_confirmation: 'Waiting for you to confirm the server',
		ok: 'Connected',
		error: 'Failing'
	};
	const SCHEDULING_TEXT: Record<SchedulingOverrideDto, string> = {
		auto: 'Automatic',
		server: 'The server sends invitations',
		client: 'Tagmail sends invitations'
	};

	let settings = $state<CaldavSettingsDto | null>(null);
	let loadedFor = $state<number | null>(null);
	let url = $state('');
	let username = $state('');
	let password = $state('');
	let allowInsecure = $state(false);
	let scheduling = $state<SchedulingOverrideDto>('auto');
	let authservId = $state('');
	let busy = $state<string | null>(null);
	let error = $state<string | null>(null);
	let pendingHost = $state<string | null>(null);
	let dialog = $state<HTMLDialogElement>();
	let accentHex = $state('#000000');

	const mine = $derived(calendars.of(accountId));
	const writable = $derived(mine.filter((c) => c.writable));
	const hasCaldav = $derived(mine.length > 0 || settings?.confirmedHost != null);
	const others = $derived(accounts.accounts.filter((a) => a.id !== accountId));
	const credentialChanged = $derived(
		settings !== null &&
			(username.trim() !== (settings.username ?? '') || url.trim() !== (settings.url ?? ''))
	);
	const passwordRequired = $derived(
		username.trim() !== '' && !password && (!settings?.hasPassword || credentialChanged)
	);
	const schedulingText = $derived.by(() => {
		if (!settings) return '';
		const base = SCHEDULING_TEXT[settings.scheduling];
		if (settings.scheduling !== 'auto') return base;
		const found = settings.schedulingDetected;
		return `${base} (${found ? `detected: ${found}` : 'not detected yet'})`;
	});
	const trustDirty = $derived(authservId.trim() !== (settings?.authservId ?? ''));
	const suggestionOpen = $derived(
		settings !== null && settings.authservId === null && settings.authservIdSuggestion !== null
	);

	$effect(() => {
		if (loadedFor === accountId) return;
		loadedFor = accountId;
		settings = null;
		void load(accountId);
	});

	$effect(() => {
		const probe = document.createElement('canvas').getContext('2d');
		if (!probe) return;
		probe.fillStyle = getComputedStyle(document.documentElement)
			.getPropertyValue('--accent')
			.trim();
		accentHex = probe.fillStyle;
	});

	$effect(() => {
		if (!dialog) return;
		if (pendingHost && !dialog.open) dialog.showModal();
		else if (!pendingHost && dialog.open) dialog.close();
	});

	function fill(s: CaldavSettingsDto) {
		settings = s;
		url = s.url ?? '';
		username = s.username ?? '';
		password = '';
		allowInsecure = s.allowInsecure;
		scheduling = s.scheduling;
		authservId = s.authservId ?? '';
	}

	async function load(id: number) {
		await run('load', async () => {
			const [s] = await Promise.all([api.caldavSettings(id), calendars.load()]);
			if (loadedFor === id) fill(s);
		});
	}

	async function run(key: string, work: () => Promise<void>): Promise<boolean> {
		busy = key;
		error = null;
		try {
			await work();
			return true;
		} catch (e) {
			error = errorText(e);
			return false;
		} finally {
			busy = null;
		}
	}

	function inputFrom(s: CaldavSettingsDto, change: Partial<CaldavSettingsInputDto>) {
		return {
			url: s.url,
			username: s.username,
			allowInsecure: s.allowInsecure,
			scheduling: s.scheduling,
			fallbackAccountId: s.fallbackAccountId,
			defaultCalendarHref: s.defaultCalendarHref,
			...change
		} satisfies CaldavSettingsInputDto;
	}

	async function put(change: Partial<CaldavSettingsInputDto>) {
		if (!settings) return false;
		const body = inputFrom(settings, change);
		return run('save', async () => fill(await api.saveCaldavSettings(accountId, body)));
	}

	function saveConnection(event: SubmitEvent) {
		event.preventDefault();
		return put(connectionChange()).then((ok) => ok && toasts.show('Saved calendar settings'));
	}

	async function discover() {
		if (!settings) return;
		if (credentialChanged || allowInsecure !== settings.allowInsecure) {
			if (passwordRequired || !(await put(connectionChange()))) return;
		}
		await run('discover', async () => {
			await takeDiscovery(await api.discoverCaldav(accountId));
		});
	}

	function connectionChange(): Partial<CaldavSettingsInputDto> {
		return {
			url: url.trim() || null,
			username: username.trim() || null,
			allowInsecure,
			scheduling,
			...(password ? { password } : {})
		};
	}

	async function takeDiscovery(found: CaldavDiscoveryDto) {
		fill(await api.caldavSettings(accountId));
		if (found.status === 'needs_confirmation') pendingHost = found.host;
		else {
			pendingHost = null;
			await calendars.load();
		}
	}

	async function confirmHost() {
		const host = pendingHost;
		if (!host) return;
		await run('confirm', async () => {
			await takeDiscovery(await api.confirmCaldavHost(accountId, host));
		});
		pendingHost = null;
	}

	function saveTrust(value: string | null) {
		void put({ authservId: value }).then((ok) => {
			if (ok)
				toasts.show(
					value ? 'Automatic updates are on for this sender id' : 'Automatic updates are off'
				);
		});
	}

	function setDefault(href: string) {
		void run('default', async () => {
			await calendars.patch({ accountId, href, default: true });
			if (settings) fill(await api.caldavSettings(accountId));
		});
	}

	function patchCalendar(href: string, change: { color?: string; visible?: boolean }) {
		void run('calendar', () => calendars.patch({ accountId, href, ...change }));
	}
</script>

<section aria-labelledby="caldav-title" class="caldav">
	<h2 id="caldav-title">Calendar</h2>
	{#if error}<p class="error" role="alert">{error}</p>{/if}
	{#if !settings}
		<p class="muted">{busy === 'load' ? 'Loading…' : 'Calendar settings are not available.'}</p>
	{:else}
		<dl class="facts" aria-label="Calendar status">
			<div>
				<dt>Status</dt>
				<dd data-caldav-status={settings.status}>
					<span class="dot" data-status={settings.status}></span>{STATUS_TEXT[settings.status]}
				</dd>
			</div>
			<div>
				<dt>Invitations</dt>
				<dd>
					{schedulingText}
				</dd>
			</div>
			{#if settings.confirmedHost}
				<div>
					<dt>Server</dt>
					<dd>{settings.confirmedHost}</dd>
				</div>
			{/if}
		</dl>
		{#if settings.lastError}<p class="last-error">{settings.lastError}</p>{/if}

		<form class="fields" onsubmit={saveConnection}>
			<label class="wide">
				<span>Calendar server URL</span>
				<input
					bind:value={url}
					placeholder="Found automatically from your address"
					spellcheck="false"
					autocomplete="off"
				/>
			</label>
			<label>
				<span>Login (if not your mail login)</span>
				<input
					bind:value={username}
					placeholder="Same as mail"
					spellcheck="false"
					autocomplete="off"
				/>
			</label>
			<label>
				<span>Calendar password</span>
				<input
					type="password"
					bind:value={password}
					placeholder={username.trim() === ''
						? 'Mail password'
						: settings.hasPassword && !credentialChanged
							? 'Unchanged'
							: ''}
					autocomplete="new-password"
					disabled={username.trim() === ''}
				/>
				{#if passwordRequired}
					<small>Enter the password again; it is not kept when the login or URL changes</small>
				{/if}
			</label>
			<label>
				<span>Invitations are sent by</span>
				<select bind:value={scheduling}>
					<option value="auto">Automatic</option>
					<option value="server">The server</option>
					<option value="client">Tagmail</option>
				</select>
			</label>
			<label class="check wide">
				<input type="checkbox" bind:checked={allowInsecure} />
				<span>Allow an unencrypted (http) connection</span>
			</label>
			{#if allowInsecure}
				<p class="warning wide" role="alert">
					<Icon name="alert" size={16} />Your calendar login and events can be read on the network.
					Use this only on a trusted local network.
				</p>
			{/if}
			<div class="actions wide">
				<button class="btn primary" type="submit" disabled={busy !== null || passwordRequired}>
					Save calendar
				</button>
				<button
					class="btn"
					type="button"
					onclick={discover}
					disabled={busy !== null || passwordRequired}
				>
					<Icon name="sync" size={16} />{busy === 'discover' ? 'Looking…' : 'Discover'}
				</button>
			</div>
		</form>

		<section class="block" aria-labelledby="trust-title">
			<h3 id="trust-title">Automatic updates</h3>
			<p class="muted">
				Tagmail applies invitation updates and cancellations by itself only for mail your provider
				vouches for. Until you confirm the sender id below, nothing is applied automatically.
			</p>
			{#if suggestionOpen}
				<p class="suggestion">
					Suggested: <code data-suggestion>{settings.authservIdSuggestion}</code>
					<button
						class="btn"
						type="button"
						disabled={busy !== null}
						onclick={() => saveTrust(settings?.authservIdSuggestion ?? null)}
					>
						Confirm
					</button>
				</p>
			{/if}
			<div class="trust">
				<label>
					<span>Trusted sender id</span>
					<input
						bind:value={authservId}
						placeholder="Empty: updates need your approval"
						spellcheck="false"
						autocomplete="off"
					/>
				</label>
				<button
					class="btn"
					type="button"
					disabled={busy !== null || !trustDirty}
					onclick={() => saveTrust(authservId.trim() || null)}
				>
					{authservId.trim() || !settings.authservId ? 'Save id' : 'Turn off'}
				</button>
			</div>
		</section>

		{#if hasCaldav}
			<div class="block">
				<h3>Calendars</h3>
				<label class="default">
					<span>Invitations go to</span>
					<select
						value={settings.defaultCalendarHref ?? ''}
						disabled={busy !== null}
						onchange={(e) => setDefault(e.currentTarget.value)}
						aria-label="Default calendar"
					>
						{#if !writable.some((c) => c.isDefault)}<option value="" disabled>Choose…</option>{/if}
						{#each writable as c (c.href)}
							<option value={c.href}>{c.displayName ?? c.href}</option>
						{/each}
					</select>
				</label>
				<ul class="items" aria-label="Calendars">
					{#each mine as c (c.href)}
						<li data-calendar={c.href}>
							<input
								type="color"
								value={c.color ?? accentHex}
								aria-label="Colour of {c.displayName ?? c.href}"
								onchange={(e) => patchCalendar(c.href, { color: e.currentTarget.value })}
							/>
							<span class="what">
								<strong>{c.displayName ?? c.href}</strong>
								{#if !c.writable}<span class="muted">Read only</span>{/if}
							</span>
							{#if c.isDefault}<span class="badge">Default</span>{/if}
							<label class="check">
								<input
									type="checkbox"
									checked={c.visible}
									onchange={(e) => patchCalendar(c.href, { visible: e.currentTarget.checked })}
								/>
								<span>Show</span>
							</label>
						</li>
					{/each}
				</ul>
			</div>
		{:else}
			<div class="block">
				<h3>Without a calendar here</h3>
				<label class="default">
					<span>Invitations go to the calendar of</span>
					<select
						value={settings.fallbackAccountId ?? ''}
						disabled={busy !== null}
						onchange={(e) =>
							void put({
								fallbackAccountId: e.currentTarget.value ? Number(e.currentTarget.value) : null
							})}
						aria-label="Fallback account"
					>
						<option value="">No account</option>
						{#each others as a (a.id)}
							<option value={a.id}>{a.name}</option>
						{/each}
					</select>
				</label>
			</div>
		{/if}
	{/if}
</section>

<dialog
	bind:this={dialog}
	aria-labelledby="caldav-host-title"
	data-caldav="confirm-host"
	onclose={() => (pendingHost = null)}
>
	<h2 id="caldav-host-title">
		Send your {settings?.username ? 'calendar' : 'mail'} login to {pendingHost}?
	</h2>
	<p>
		This calendar server is on <strong data-host>{pendingHost}</strong>, not your mail domain.
		Confirming sends your {settings?.username ? 'calendar' : 'mail'} username and password to it.
	</p>
	<p>Only continue if you recognise this host.</p>
	<div class="actions end">
		<button class="btn" type="button" onclick={() => (pendingHost = null)}>Cancel</button>
		<button class="btn primary" type="button" disabled={busy !== null} onclick={confirmHost}>
			Send login to {pendingHost}
		</button>
	</div>
</dialog>

<style>
	.caldav {
		max-width: 820px;
		margin-bottom: 30px;
	}

	h2 {
		margin: 0 0 10px;
		font-size: 15px;
		font-weight: 650;
	}

	h3 {
		margin: 0 0 6px;
		font-size: 14px;
		font-weight: 650;
	}

	.facts {
		display: grid;
		gap: 6px;
		margin: 0 0 14px;
	}

	.facts div {
		display: flex;
		gap: 12px;
		font-size: 14px;
	}

	dt {
		width: 100px;
		color: var(--muted);
	}

	dd {
		margin: 0;
	}

	.dot {
		display: inline-block;
		width: 8px;
		height: 8px;
		margin-right: 8px;
		border-radius: 50%;
		background: var(--faint);
	}

	.dot[data-status='ok'] {
		background: var(--ok);
	}

	.dot[data-status='error'] {
		background: var(--danger);
	}

	.dot[data-status='needs_confirmation'] {
		background: var(--star);
	}

	.last-error {
		margin: 0 0 14px;
		padding: 8px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
		font-size: 13px;
	}

	.fields {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 14px 16px;
		max-width: 640px;
	}

	.wide {
		grid-column: 1 / -1;
	}

	label {
		display: grid;
		gap: 5px;
		min-width: 0;
		color: var(--muted);
		font-size: 13px;
	}

	label.check {
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--ink);
	}

	input:not([type='checkbox'], [type='color']),
	select {
		height: 36px;
		padding: 0 12px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface-2);
		color: var(--ink);
		font-size: 14px;
	}

	input:disabled {
		opacity: 0.55;
	}

	small {
		color: var(--muted);
	}

	.warning {
		display: flex;
		align-items: flex-start;
		gap: 8px;
		margin: 0;
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
		font-size: 13px;
	}

	.actions {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
	}

	.actions.end {
		justify-content: flex-end;
		margin-top: 16px;
	}

	.block {
		margin-top: 22px;
	}

	.suggestion {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
		margin: 0 0 10px;
		font-size: 13px;
	}

	code {
		font-family: var(--font-mono);
	}

	.trust {
		display: flex;
		align-items: end;
		gap: 10px;
		max-width: 640px;
	}

	.trust label {
		flex: 1;
	}

	.default {
		max-width: 320px;
		margin-bottom: 12px;
	}

	.items {
		margin: 0;
		padding: 0;
		overflow: hidden;
		border: 1px solid var(--line);
		border-radius: var(--radius);
		list-style: none;
	}

	.items li {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 10px 16px;
		border-bottom: 1px solid var(--line);
	}

	.items li:last-child {
		border-bottom: 0;
	}

	.items input[type='color'] {
		width: 28px;
		height: 28px;
		padding: 0;
		border: 1px solid var(--line-strong);
		border-radius: 50%;
		background: none;
		cursor: pointer;
	}

	.what {
		display: grid;
		flex: 1;
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

	.muted {
		color: var(--muted);
		font-size: 13px;
	}

	.error {
		padding: 10px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
	}

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

	@media (prefers-color-scheme: dark) {
		dialog::backdrop {
			background: rgb(0 0 0 / 0.7);
		}
	}

	dialog h2 {
		margin: 0 0 8px;
		font-size: 18px;
		font-weight: 650;
		overflow-wrap: anywhere;
	}

	dialog strong {
		overflow-wrap: anywhere;
	}

	@media (max-width: 720px) {
		.fields {
			grid-template-columns: minmax(0, 1fr);
		}

		.trust {
			flex-direction: column;
			align-items: stretch;
		}
	}
</style>
