<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { labelHue } from '../format';
	import { currentAccount, nav } from '../stores';
	import { withAccount } from '../views';

	let { open = $bindable(false) }: { open?: boolean } = $props();

	let dialog: HTMLDialogElement;
	let input: HTMLInputElement | undefined = $state();
	let filter = $state('');
	let active = $state(0);

	const account = $derived(currentAccount(page.url));
	const matches = $derived.by(() => {
		const f = filter.trim().toLowerCase();
		const own =
			account === null ? nav.labels : nav.labels.filter((l) => l.accountIds.includes(account));
		return f ? own.filter((l) => l.name.toLowerCase().includes(f)) : own;
	});

	$effect(() => {
		if (open && !dialog.open) {
			filter = '';
			active = 0;
			dialog.showModal();
			input?.focus();
		} else if (!open && dialog.open) dialog.close();
	});

	$effect(() => {
		void filter;
		active = 0;
	});

	function go(key: string) {
		open = false;
		void goto(withAccount(`/label/${encodeURIComponent(key)}`, account));
	}

	function onkeydown(event: KeyboardEvent) {
		if (event.key === 'ArrowDown') active = Math.min(matches.length - 1, active + 1);
		else if (event.key === 'ArrowUp') active = Math.max(0, active - 1);
		else if (event.key === 'Enter' && matches[active]) go(matches[active].key);
		else return;
		event.preventDefault();
	}
</script>

<dialog
	bind:this={dialog}
	aria-label="Go to label"
	onclose={() => (open = false)}
	onclick={(e) => {
		if (e.target === dialog) open = false;
	}}
>
	<input
		bind:this={input}
		bind:value={filter}
		placeholder="Go to label…"
		aria-label="Label name"
		role="combobox"
		aria-expanded="true"
		aria-controls="label-jump-list"
		aria-activedescendant={matches[active] ? `jump-${active}` : undefined}
		autocomplete="off"
		{onkeydown}
	/>
	<ul id="label-jump-list" role="listbox" aria-label="Labels">
		{#each matches as label, i (label.key)}
			<li
				id="jump-{i}"
				role="option"
				aria-selected={i === active}
				class:active={i === active}
				onmousedown={(e) => e.preventDefault()}
				onclick={() => go(label.key)}
				onkeydown={() => {}}
			>
				<span class="dot" style:--h={labelHue(label.key)}></span>{label.name}
			</li>
		{:else}
			<li class="none" role="presentation">
				{nav.labels.length === 0 ? 'No labels yet' : 'No label matches'}
			</li>
		{/each}
	</ul>
</dialog>

<style>
	dialog {
		width: min(420px, calc(100vw - 32px));
		margin-top: 14vh;
		padding: 8px;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	dialog::backdrop {
		background: rgb(8 10 14 / 0.35);
	}

	input {
		width: 100%;
		height: 44px;
		padding: 0 12px;
		border: 0;
		border-bottom: 1px solid var(--line);
		outline: none;
		background: transparent;
		font-size: 16px;
	}

	ul {
		max-height: 50vh;
		margin: 6px 0 0;
		padding: 0;
		overflow-y: auto;
		list-style: none;
	}

	li {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 9px 12px;
		border-radius: 8px;
		cursor: pointer;
	}

	li.active {
		background: var(--select);
	}

	.none {
		color: var(--muted);
		cursor: default;
	}

	.dot {
		width: 10px;
		height: 10px;
		border-radius: 3px;
		background: oklch(0.65 0.14 var(--h));
	}
</style>
