<script lang="ts">
	import type { AccountTestDto, TlsMode } from '@tagmail/api-types';
	import { PORTS, emailFromUsername, type AccountForm } from './accountForm';
	import Icon from './Icon.svelte';

	let {
		form = $bindable(),
		errors = {},
		test = null,
		creating = false,
		passwordRequired = false
	}: {
		form: AccountForm;
		/** Messages by field name, from a 422 answer. */
		errors?: Record<string, string>;
		test?: AccountTestDto | null;
		/** A new account: the password is required and the email follows the username. */
		creating?: boolean;
		/** An existing account whose server or username changed. */
		passwordRequired?: boolean;
	} = $props();

	let emailEdited = $state(false);

	function setUsername(value: string) {
		form.username = value;
		if (creating && !emailEdited) form.email = emailFromUsername(value);
	}

	function setTls(kind: 'imap' | 'smtp', mode: TlsMode) {
		const ports = PORTS[kind];
		const port = kind === 'imap' ? form.imapPort : form.smtpPort;
		const next = port === ports.tls || port === ports.starttls ? ports[mode] : port;
		if (kind === 'imap') {
			form.imapTls = mode;
			form.imapPort = next;
		} else {
			form.smtpTls = mode;
			form.smtpPort = next;
		}
	}
</script>

<div class="fields">
	<label class="wide">
		<span>Account name</span>
		<input bind:value={form.name} required placeholder="Personal" autocomplete="off" />
		{#if errors.name}<em role="alert">{errors.name}</em>{/if}
	</label>
	<label>
		<span>Username</span>
		<input
			value={form.username}
			oninput={(e) => setUsername(e.currentTarget.value)}
			required
			autocomplete="off"
			spellcheck="false"
		/>
		{#if errors.username}<em role="alert">{errors.username}</em>{/if}
	</label>
	<label>
		<span>Password</span>
		<input
			type="password"
			bind:value={form.password}
			required={creating || passwordRequired}
			placeholder={creating ? '' : 'Unchanged'}
			autocomplete="new-password"
		/>
		{#if passwordRequired}
			<small>Re-enter the password when the server or username changes</small>
		{/if}
		{#if errors.password}<em role="alert">{errors.password}</em>{/if}
	</label>
	<label>
		<span>Send as (email address)</span>
		<input
			type="email"
			bind:value={form.email}
			oninput={() => (emailEdited = true)}
			required
			spellcheck="false"
		/>
		{#if errors.email}<em role="alert">{errors.email}</em>{/if}
	</label>
	{#if creating}
		<label>
			<span>Your name on sent mail</span>
			<input bind:value={form.displayName} autocomplete="name" />
		</label>
	{/if}

	<fieldset>
		<legend>Incoming (IMAP)</legend>
		<label class="host">
			<span>Server</span>
			<input
				bind:value={form.imapHost}
				required
				placeholder="imap.example.com"
				spellcheck="false"
			/>
			{#if errors.imapHost}<em role="alert">{errors.imapHost}</em>{/if}
		</label>
		<label class="port">
			<span>Port</span>
			<input type="number" min="1" max="65535" bind:value={form.imapPort} required />
		</label>
		<label class="port">
			<span>Security</span>
			<select
				value={form.imapTls}
				onchange={(e) => setTls('imap', e.currentTarget.value as TlsMode)}
				aria-label="IMAP security"
			>
				<option value="tls">TLS</option>
				<option value="starttls">STARTTLS</option>
			</select>
		</label>
	</fieldset>
	<fieldset>
		<legend>Outgoing (SMTP)</legend>
		<label class="host">
			<span>Server</span>
			<input
				bind:value={form.smtpHost}
				required
				placeholder="smtp.example.com"
				spellcheck="false"
			/>
			{#if errors.smtpHost}<em role="alert">{errors.smtpHost}</em>{/if}
		</label>
		<label class="port">
			<span>Port</span>
			<input type="number" min="1" max="65535" bind:value={form.smtpPort} required />
		</label>
		<label class="port">
			<span>Security</span>
			<select
				value={form.smtpTls}
				onchange={(e) => setTls('smtp', e.currentTarget.value as TlsMode)}
				aria-label="SMTP security"
			>
				<option value="tls">TLS</option>
				<option value="starttls">STARTTLS</option>
			</select>
		</label>
	</fieldset>
</div>

{#if test}
	<ul class="test" aria-label="Connection test">
		{#each [{ name: 'IMAP', step: test.imap }, { name: 'SMTP', step: test.smtp }] as { name, step } (name)}
			<li class:ok={step.ok}>
				<Icon name={step.ok ? 'check' : 'alert'} size={16} />
				<strong>{name}</strong>
				{#if step.ok}
					<span>
						Connected in {step.ms} ms{#if name === 'IMAP' && test.mailboxes !== null}, {test.mailboxes}
							folders{/if}
					</span>
				{:else}
					<span>{step.error ?? 'Failed'}</span>
				{/if}
			</li>
		{/each}
	</ul>
{/if}

<style>
	.fields {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 14px 16px;
		max-width: 640px;
	}

	.wide,
	fieldset {
		grid-column: 1 / -1;
	}

	label {
		display: grid;
		gap: 5px;
		min-width: 0;
		font-size: 13px;
		color: var(--muted);
	}

	input,
	select {
		height: 36px;
		padding: 0 12px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface-2);
		color: var(--ink);
		font-size: 14px;
	}

	small {
		color: var(--muted);
		font-size: 12.5px;
	}

	em {
		color: var(--danger);
		font-size: 12.5px;
		font-style: normal;
	}

	fieldset {
		display: grid;
		grid-template-columns: minmax(0, 1fr) 96px 128px;
		gap: 10px;
		margin: 0;
		padding: 12px 14px 14px;
		border: 1px solid var(--line);
		border-radius: var(--radius);
	}

	legend {
		padding: 0 6px;
		color: var(--ink-2);
		font-size: 13px;
		font-weight: 650;
	}

	.test {
		display: grid;
		gap: 6px;
		max-width: 640px;
		margin: 14px 0 0;
		padding: 0;
		list-style: none;
	}

	.test li {
		display: flex;
		align-items: center;
		gap: 8px;
		padding: 8px 12px;
		border-radius: 10px;
		background: var(--danger-soft);
		color: var(--danger);
		font-size: 13px;
	}

	.test li.ok {
		background: color-mix(in oklab, var(--ok) 14%, var(--surface));
		color: var(--ok);
	}

	.test span {
		color: var(--ink-2);
	}

	@media (max-width: 720px) {
		.fields {
			grid-template-columns: minmax(0, 1fr);
		}

		fieldset {
			grid-template-columns: minmax(0, 1fr) 90px;
		}

		fieldset .host {
			grid-column: 1 / -1;
		}
	}
</style>
