<script lang="ts">
	import { originsOf, pickers } from '../ops/actions.svelte';
	import { isInbox, mailboxLabel, nav, ops } from '../stores';
	import type { MailboxDto } from '../transport';
	import Icon, { type IconName } from './Icon.svelte';

	let dialog: HTMLDialogElement;
	let input: HTMLInputElement | undefined = $state();
	let filter = $state('');
	let active = $state(0);

	const target = $derived(pickers.move);
	const current = $derived(
		target?.view?.startsWith('mailbox:') ? Number(target.view.slice('mailbox:'.length)) : null
	);
	const matches = $derived.by(() => {
		const f = filter.trim().toLowerCase();
		return nav.mailboxes.filter(
			(m) => m.id !== current && (!f || displayName(m).toLowerCase().includes(f))
		);
	});

	function displayName(m: MailboxDto): string {
		return mailboxLabel(m);
	}

	function icon(m: MailboxDto): IconName {
		if (isInbox(m)) return 'inbox';
		if (m.specialUse === 'trash') return 'trash';
		if (m.specialUse === 'archive' || m.specialUse === 'all') return 'archive';
		if (m.specialUse === 'junk') return 'alert';
		if (m.specialUse === 'drafts') return 'pencil';
		if (m.specialUse === 'sent') return 'send';
		return 'folder';
	}

	$effect(() => {
		if (target && !dialog.open) {
			filter = '';
			active = 0;
			dialog.showModal();
			input?.focus();
		} else if (!target && dialog.open) dialog.close();
	});

	$effect(() => {
		void filter;
		active = 0;
	});

	function close() {
		pickers.move = null;
	}

	function move(m: MailboxDto) {
		const t = target;
		if (!t) return;
		const kind = isInbox(m) ? 'unarchive' : m.specialUse === 'trash' ? 'trash' : 'move';
		ops.run(
			kind === 'move'
				? { kind, mailboxId: m.id, threadIds: t.threadIds }
				: { kind, threadIds: t.threadIds },
			{ view: t.view, origins: originsOf(t.rows) }
		);
		close();
	}

	function onkeydown(event: KeyboardEvent) {
		if (event.key === 'ArrowDown') active = Math.min(matches.length - 1, active + 1);
		else if (event.key === 'ArrowUp') active = Math.max(0, active - 1);
		else if (event.key === 'Enter' && matches[active]) move(matches[active]);
		else return;
		event.preventDefault();
	}
</script>

<dialog
	bind:this={dialog}
	aria-labelledby="move-picker-title"
	onclose={close}
	onclick={(e) => {
		if (e.target === dialog) close();
	}}
>
	<header>
		<h2 id="move-picker-title">Move to</h2>
		{#if target}
			<span class="count">
				{target.threadIds.length} conversation{target.threadIds.length === 1 ? '' : 's'}
			</span>
		{/if}
	</header>
	<div class="search">
		<Icon name="search" size={18} />
		<input
			bind:this={input}
			bind:value={filter}
			placeholder="Find a folder"
			aria-label="Folder name"
			role="combobox"
			aria-expanded="true"
			aria-controls="move-picker-list"
			aria-activedescendant={matches[active] ? `move-${active}` : undefined}
			autocomplete="off"
			{onkeydown}
		/>
	</div>
	<ul id="move-picker-list" role="listbox" aria-label="Folders">
		{#each matches as box, i (box.id)}
			<li
				id="move-{i}"
				role="option"
				aria-selected={i === active}
				class:active={i === active}
				onmousedown={(e) => e.preventDefault()}
				onclick={() => move(box)}
				onkeydown={() => {}}
			>
				<Icon name={icon(box)} size={18} />
				<span>{displayName(box)}</span>
			</li>
		{:else}
			<li class="none" role="presentation">No folder matches</li>
		{/each}
	</ul>
</dialog>

<style>
	dialog {
		width: min(380px, calc(100vw - 32px));
		margin-top: 12vh;
		padding: 0 0 8px;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	dialog::backdrop {
		background: rgb(8 10 14 / 0.35);
	}

	header {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		padding: 16px 18px 8px;
	}

	h2 {
		margin: 0;
		font-size: 15px;
		font-weight: 650;
	}

	.count {
		color: var(--muted);
		font-family: var(--font-mono);
		font-size: 12px;
	}

	.search {
		display: flex;
		align-items: center;
		gap: 8px;
		margin: 0 12px;
		padding: 0 10px;
		border: 1px solid var(--line-strong);
		border-radius: 10px;
		background: var(--surface-2);
		color: var(--muted);
	}

	.search:focus-within {
		border-color: var(--accent);
		box-shadow: 0 0 0 3px var(--accent-soft);
	}

	input {
		flex: 1;
		height: 38px;
		border: 0;
		outline: none;
		background: transparent;
		color: var(--ink);
	}

	ul {
		max-height: min(46vh, 360px);
		margin: 8px 0 0;
		padding: 0 6px;
		overflow-y: auto;
		list-style: none;
	}

	li {
		display: flex;
		align-items: center;
		gap: 12px;
		height: 38px;
		padding: 0 10px;
		border-radius: 8px;
		color: var(--ink-2);
		cursor: pointer;
	}

	li.active {
		background: var(--hover);
		box-shadow: inset 2px 0 0 var(--accent);
		color: var(--ink);
	}

	.none {
		color: var(--muted);
		cursor: default;
	}
</style>
