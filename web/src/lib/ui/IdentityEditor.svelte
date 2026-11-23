<script module lang="ts">
	export interface IdentityInput {
		address: string;
		name: string;
		/** Sanitized on the server; `''` clears it. */
		signature: string;
	}
</script>

<script lang="ts">
	import type { DOMPurify } from 'dompurify';
	import { onMount } from 'svelte';
	import Editor from '../compose/Editor.svelte';
	import { sanitizeHtml } from '../compose/sanitize';
	import type { IdentityDto } from '../transport';

	let {
		identity = null,
		busy = false,
		error = null,
		onsave,
		oncancel
	}: {
		/** `null` adds a new identity. */
		identity?: IdentityDto | null;
		busy?: boolean;
		error?: string | null;
		onsave: (input: IdentityInput) => void;
		oncancel: () => void;
	} = $props();

	// The editor is keyed on the identity; later prop changes start a new one.
	// svelte-ignore state_referenced_locally
	let address = $state(identity?.address ?? '');
	// svelte-ignore state_referenced_locally
	let name = $state(identity?.name ?? '');
	// svelte-ignore state_referenced_locally
	let signature = $state(identity?.signature ?? '');
	let purify = $state.raw<DOMPurify | null>(null);

	const preview = $derived(purify && signature ? sanitizeHtml(purify, signature) : '');

	onMount(() => {
		void import('dompurify').then((m) => (purify = m.default));
	});

	function submit(event: SubmitEvent) {
		event.preventDefault();
		onsave({ address: address.trim(), name: name.trim(), signature });
	}
</script>

<form class="identity" onsubmit={submit}>
	<div class="row">
		<label>
			<span>Address</span>
			<input type="email" bind:value={address} required spellcheck="false" />
		</label>
		<label>
			<span>Name</span>
			<input bind:value={name} placeholder="Shown next to the address" />
		</label>
	</div>
	<div class="sig">
		<span class="caption">Signature</span>
		<div class="box">
			<Editor
				html={signature}
				label="Signature"
				onchange={(html, text) => (signature = text.trim() ? html : '')}
			/>
		</div>
	</div>
	{#if preview}
		<div class="sig">
			<span class="caption">Preview</span>
			<div class="preview" data-testid="signature-preview">
				<span class="delim">--&nbsp;</span>
				<!-- eslint-disable-next-line svelte/no-at-html-tags -- sanitized with the compose policy -->
				{@html preview}
			</div>
		</div>
	{/if}
	{#if error}<p class="error" role="alert">{error}</p>{/if}
	<div class="actions">
		<button class="btn primary" type="submit" disabled={busy || !address.trim()}>
			{identity ? 'Save identity' : 'Add identity'}
		</button>
		<button class="btn" type="button" onclick={oncancel}>Cancel</button>
	</div>
</form>

<style>
	.identity {
		display: grid;
		gap: 12px;
		padding: 14px 16px 16px;
		background: var(--surface-2);
	}

	.row {
		display: grid;
		grid-template-columns: repeat(2, minmax(0, 1fr));
		gap: 12px;
	}

	label,
	.sig {
		display: grid;
		gap: 5px;
		min-width: 0;
	}

	label span,
	.caption {
		color: var(--muted);
		font-size: 13px;
	}

	input {
		height: 36px;
		padding: 0 12px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface);
		color: var(--ink);
	}

	.box {
		display: flex;
		min-height: 150px;
		overflow: hidden;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface);
	}

	.preview {
		padding: 10px 14px;
		border: 1px dashed var(--line-strong);
		border-radius: 10px;
		color: var(--ink-2);
		line-height: 1.5;
	}

	.delim {
		display: block;
		color: var(--muted);
	}

	.actions {
		display: flex;
		gap: 10px;
	}

	.error {
		margin: 0;
		color: var(--danger);
	}

	@media (max-width: 720px) {
		.row {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
