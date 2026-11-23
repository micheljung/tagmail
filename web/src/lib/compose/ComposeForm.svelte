<script lang="ts">
	import { onMount, untrack } from 'svelte';
	import { fileSize } from '../format';
	import { accounts } from '../stores';
	import type { IdentityDto } from '../transport';
	import Icon from '../ui/Icon.svelte';
	import { composer } from './composer.svelte';
	import type { Draft } from './draft.svelte';
	import Editor from './Editor.svelte';
	import RecipientField from './RecipientField.svelte';
	import { findSignature, insertSignature, signatureBlock, swapSignature } from './signature';

	let {
		draft,
		inline = false,
		onsend,
		ondiscard
	}: {
		draft: Draft;
		inline?: boolean;
		onsend: () => void;
		ondiscard: () => void;
	} = $props();

	let fileInput: HTMLInputElement;
	let formEl: HTMLFormElement;
	let editor: Editor | undefined = $state();
	let toField: RecipientField | undefined = $state();
	let dragging = $state(0);
	let tried = $state(false);
	let editorReady = $state(false);
	/** A reply or forward: the signature goes above the quoted original. */
	const quoting = $derived(draft.mode !== 'new');

	/** Identities per account, in the server's order, for the From `<optgroup>`s. */
	const fromGroups = $derived.by(() => {
		const groups = new Map<number, IdentityDto[]>();
		for (const i of composer.identities)
			groups.set(i.accountId, [...(groups.get(i.accountId) ?? []), i]);
		return [...groups].map(([accountId, identities]) => ({ accountId, identities }));
	});

	$effect(() => {
		if (!editorReady || draft.signatureChecked) return;
		const identity = composer.identities.find((i) => i.id === draft.identityId);
		if (!identity && draft.draftId === null) return;
		untrack(() => addInitialSignature(identity));
	});

	const status = $derived(
		draft.saveState === 'saving'
			? 'Saving…'
			: draft.saveState === 'saved'
				? 'Draft saved'
				: draft.saveState === 'error'
					? "Couldn't save the draft"
					: ''
	);

	$effect(() => {
		const onkeydown = (e: KeyboardEvent) => {
			if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
				e.preventDefault();
				send();
			}
		};
		formEl.addEventListener('keydown', onkeydown);
		return () => formEl.removeEventListener('keydown', onkeydown);
	});

	onMount(() => {
		if (!inline) requestAnimationFrame(focus);
	});

	$effect(() => {
		if (composer.identities.length === 0) void composer.loadIdentities().catch(() => undefined);
	});

	export function focus(): void {
		if (draft.to.length === 0) toField?.focus();
		else editor?.focus();
	}

	function send() {
		tried = true;
		if (draft.sendBlocker) return;
		onsend();
	}

	function ondrop(event: DragEvent) {
		event.preventDefault();
		dragging = 0;
		const files = event.dataTransfer?.files;
		if (files && files.length > 0) draft.attach(files);
	}

	function hasFiles(event: DragEvent): boolean {
		return event.dataTransfer?.types.includes('Files') ?? false;
	}

	/** A new message gets its identity's signature; a reopened draft keeps what it has. */
	function addInitialSignature(identity: IdentityDto | undefined) {
		draft.signatureChecked = true;
		const body = editor?.getHTML() ?? '';
		const block = identity && draft.draftId === null ? signatureBlock(identity) : null;
		if (!block) {
			draft.signature = findSignature(body)?.inner ?? null;
			return;
		}
		replaceBody(insertSignature(body, block, quoting), false);
	}

	function replaceBody(html: string, edited: boolean) {
		if (!editor) return;
		const shown = editor.setHTML(html);
		draft.html = shown.html;
		draft.signature = findSignature(shown.html)?.inner ?? null;
		if (edited) draft.text = shown.text;
	}

	function changeFrom(id: number) {
		draft.identityId = id;
		const identity = composer.identities.find((i) => i.id === id);
		if (identity && editor && editorReady) {
			const next = swapSignature(editor.getHTML(), draft.signature, identity, quoting);
			if (next !== null) replaceBody(next, true);
		}
		draft.touch();
	}

	function identityLabel(id: number): string {
		const i = composer.identities.find((x) => x.id === id);
		if (!i) return '';
		return i.name ? `${i.name} <${i.address}>` : i.address;
	}
</script>

<form
	bind:this={formEl}
	class="form"
	class:inline
	class:dragging={dragging > 0}
	onsubmit={(e) => {
		e.preventDefault();
		send();
	}}
	ondragenter={(e) => {
		if (hasFiles(e)) dragging++;
	}}
	ondragleave={(e) => {
		if (hasFiles(e)) dragging = Math.max(0, dragging - 1);
	}}
	ondragover={(e) => {
		if (hasFiles(e)) e.preventDefault();
	}}
	{ondrop}
>
	{#if composer.identities.length > 1}
		<label class="from">
			<span class="label">From</span>
			<select
				value={draft.identityId}
				onchange={(e) => changeFrom(Number(e.currentTarget.value))}
				aria-label="From"
			>
				{#if fromGroups.length > 1}
					{#each fromGroups as group (group.accountId)}
						<optgroup label={accounts.name(group.accountId)}>
							{#each group.identities as identity (identity.id)}
								<option value={identity.id}>{identityLabel(identity.id)}</option>
							{/each}
						</optgroup>
					{/each}
				{:else}
					{#each composer.identities as identity (identity.id)}
						<option value={identity.id}>{identityLabel(identity.id)}</option>
					{/each}
				{/if}
			</select>
		</label>
	{:else if composer.identities.length === 1}
		<div class="from">
			<span class="label">From</span>
			<span class="only">{identityLabel(composer.identities[0].id)}</span>
		</div>
	{/if}

	<RecipientField
		bind:this={toField}
		label="To"
		recipients={draft.to}
		onadd={(t) => draft.addRecipients('to', t)}
		onremove={(i) => draft.removeRecipient('to', i)}
	>
		<span class="extra">
			{#if !draft.showCc}<button type="button" onclick={() => (draft.showCc = true)}>
					Cc
				</button>{/if}
			{#if !draft.showBcc}<button type="button" onclick={() => (draft.showBcc = true)}>
					Bcc
				</button>{/if}
		</span>
	</RecipientField>
	{#if draft.showCc}
		<RecipientField
			label="Cc"
			recipients={draft.cc}
			onadd={(t) => draft.addRecipients('cc', t)}
			onremove={(i) => draft.removeRecipient('cc', i)}
		/>
	{/if}
	{#if draft.showBcc}
		<RecipientField
			label="Bcc"
			recipients={draft.bcc}
			onadd={(t) => draft.addRecipients('bcc', t)}
			onremove={(i) => draft.removeRecipient('bcc', i)}
		/>
	{/if}

	{#if !inline || draft.mode === 'forward'}
		<input
			class="subject"
			bind:value={draft.subject}
			oninput={() => draft.touch()}
			placeholder="Subject"
			aria-label="Subject"
		/>
	{/if}

	<Editor
		bind:this={editor}
		html={draft.html}
		label="Message body"
		onchange={(html, text) => {
			draft.html = html;
			draft.text = text;
			draft.touch();
		}}
		onready={() => (editorReady = true)}
	/>

	{#if draft.uploads.length > 0}
		<ul class="files" aria-label="Attachments">
			{#each draft.uploads as u (u.key)}
				<li class={u.status}>
					<Icon name={u.status === 'error' ? 'alert' : 'clip'} size={16} />
					<span class="fname">{u.name}</span>
					<span class="fsize">
						{#if u.status === 'uploading'}{fileSize(u.loaded)} / {fileSize(u.size)}{:else}{fileSize(
								u.size
							)}{/if}
					</span>
					{#if u.status === 'error'}<span class="ferr" role="alert">{u.error}</span>{/if}
					<button
						type="button"
						aria-label="Remove {u.name}"
						onclick={() => draft.removeUpload(u.key)}
					>
						<Icon name="close" size={14} />
					</button>
					{#if u.status === 'uploading'}
						<progress max={u.size || 1} value={u.loaded} aria-label="Uploading {u.name}"></progress>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}

	{#if tried && draft.sendBlocker}
		<p class="blocker" role="alert">{draft.sendBlocker}</p>
	{/if}

	<footer>
		<button class="btn primary send" type="submit" title="Send (Ctrl+Enter)">
			Send <Icon name="send" size={16} />
		</button>
		<button
			type="button"
			class="icon"
			aria-label="Attach files"
			title="Attach files (or drop them here)"
			onclick={() => fileInput.click()}
		>
			<Icon name="clip" size={19} />
		</button>
		<input
			bind:this={fileInput}
			class="sr-only"
			type="file"
			multiple
			tabindex="-1"
			aria-hidden="true"
			onchange={() => {
				if (fileInput.files) draft.attach(fileInput.files);
				fileInput.value = '';
			}}
		/>
		<span class="status" role="status">{status}</span>
		<button
			type="button"
			class="icon"
			aria-label="Discard draft"
			title="Discard draft"
			onclick={ondiscard}
		>
			<Icon name="trash" size={19} />
		</button>
	</footer>

	{#if dragging > 0}
		<div class="drop" aria-hidden="true">
			<Icon name="clip" size={28} />
			<span>Drop files to attach</span>
		</div>
	{/if}
</form>

<style>
	.form {
		position: relative;
		display: flex;
		flex-direction: column;
		min-height: 0;
		flex: 1;
		background: var(--surface);
	}

	.from {
		display: flex;
		align-items: center;
		gap: 8px;
		min-height: 40px;
		padding: 0 12px 0 16px;
		border-bottom: 1px solid var(--line);
	}

	.label {
		width: 30px;
		color: var(--muted);
		font-size: 13px;
	}

	.from select {
		flex: 1;
		height: 32px;
		border: 0;
		background: transparent;
	}

	.only {
		color: var(--ink-2);
	}

	.extra {
		display: flex;
		gap: 2px;
		padding-top: 4px;
	}

	.extra button {
		height: 26px;
		padding: 0 8px;
		border: 0;
		border-radius: 6px;
		background: none;
		color: var(--muted);
		cursor: pointer;
	}

	.extra button:hover {
		background: var(--hover);
		color: var(--ink);
	}

	.subject {
		height: 42px;
		padding: 0 16px;
		border: 0;
		border-bottom: 1px solid var(--line);
		outline: none;
		background: transparent;
		font-weight: 560;
	}

	.files {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		margin: 0;
		padding: 8px 14px;
		list-style: none;
	}

	.files li {
		position: relative;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 6px;
		max-width: 100%;
		padding: 6px 4px 6px 10px;
		overflow: hidden;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface-2);
		font-size: 13px;
	}

	.files li.error {
		border-color: var(--danger);
		background: var(--danger-soft);
		color: var(--danger);
	}

	.fname {
		max-width: 22ch;
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
		font-weight: 560;
	}

	.fsize {
		color: var(--muted);
		font-family: var(--font-mono);
		font-size: 11.5px;
	}

	.ferr {
		font-size: 12px;
	}

	.files button {
		display: grid;
		place-items: center;
		width: 24px;
		height: 24px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: inherit;
		cursor: pointer;
	}

	.files button:hover {
		background: var(--hover);
	}

	progress {
		position: absolute;
		left: 0;
		right: 0;
		bottom: 0;
		width: 100%;
		height: 3px;
		border: 0;
		appearance: none;
		background: transparent;
	}

	progress::-webkit-progress-bar {
		background: transparent;
	}

	progress::-webkit-progress-value {
		background: var(--accent);
	}

	progress::-moz-progress-bar {
		background: var(--accent);
	}

	.blocker {
		margin: 0 14px 4px;
		color: var(--danger);
		font-size: 13px;
	}

	footer {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 10px 12px 12px;
	}

	.send {
		padding: 0 18px;
	}

	.icon {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: var(--ink-2);
		cursor: pointer;
	}

	.icon:hover {
		background: var(--hover);
	}

	.status {
		flex: 1;
		color: var(--muted);
		font-size: 12px;
		text-align: right;
	}

	.drop {
		position: absolute;
		inset: 6px;
		z-index: 3;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 8px;
		border: 2px dashed var(--accent);
		border-radius: 12px;
		background: color-mix(in oklab, var(--accent-soft) 88%, transparent);
		color: var(--accent);
		font-weight: 650;
		pointer-events: none;
	}

	.inline {
		border: 1px solid var(--line-strong);
		border-radius: 14px;
		box-shadow: var(--shadow);
		overflow: hidden;
	}
</style>
