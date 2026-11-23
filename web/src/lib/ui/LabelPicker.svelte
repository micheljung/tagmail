<script lang="ts">
	import { labelColor } from '../format';
	import { pickers, runOp } from '../ops/actions.svelte';
	import { findByName, labelChanges, labelState, nextState, type TriState } from '../ops/labels';
	import { nav, toasts } from '../stores';
	import { api } from '../transport';
	import { errorText } from './errors';
	import Icon from './Icon.svelte';

	let dialog: HTMLDialogElement;
	let input: HTMLInputElement | undefined = $state();
	let filter = $state('');
	let active = $state(0);
	let busy = $state(false);
	let error = $state<string | null>(null);
	let initial = $state.raw(new Map<string, TriState>());
	let current = $state.raw(new Map<string, TriState>());

	const target = $derived(pickers.label);
	const matches = $derived.by(() => {
		const f = filter.trim().toLowerCase();
		return f ? nav.labels.filter((l) => l.name.toLowerCase().includes(f)) : nav.labels;
	});
	const canCreate = $derived(filter.trim() !== '' && !findByName(nav.labels, filter));
	const optionCount = $derived(matches.length + (canCreate ? 1 : 0));
	const changes = $derived(labelChanges(initial, current));
	const dirty = $derived(changes.add.length + changes.remove.length > 0);

	$effect(() => {
		const t = target;
		if (t && !dialog.open) {
			const sets = t.rows.map((r) => r.labels);
			const states = new Map<string, TriState>();
			for (const l of nav.labels) states.set(l.key, labelState(l.key, sets));
			initial = states;
			current = new Map(states);
			filter = '';
			active = 0;
			error = null;
			dialog.showModal();
			input?.focus();
		} else if (!t && dialog.open) dialog.close();
	});

	$effect(() => {
		void filter;
		active = 0;
	});

	function close() {
		pickers.label = null;
	}

	function toggle(key: string) {
		const next = new Map(current);
		next.set(key, nextState(initial.get(key) ?? 'none', current.get(key) ?? 'none'));
		current = next;
	}

	function apply(final: Map<string, TriState> = current) {
		const t = target;
		if (!t) return;
		const { add, remove } = labelChanges(initial, final);
		for (const label of add) runOp({ kind: 'label.add', label }, t);
		for (const label of remove) runOp({ kind: 'label.remove', label }, t);
		close();
	}

	async function create() {
		const name = filter.trim();
		if (!name || busy) return;
		busy = true;
		error = null;
		try {
			const label = await api.createLabel({ name });
			await nav.load();
			const final = new Map(current);
			final.set(label.key, 'all');
			apply(final);
			toasts.show(`Created label “${label.name}”`);
		} catch (e) {
			error = errorText(e);
		} finally {
			busy = false;
		}
	}

	function choose(index: number) {
		if (index < matches.length) toggle(matches[index].key);
		else if (canCreate) void create();
	}

	function onkeydown(event: KeyboardEvent) {
		if (event.key === 'ArrowDown') active = Math.min(optionCount - 1, active + 1);
		else if (event.key === 'ArrowUp') active = Math.max(0, active - 1);
		else if (event.key === ' ' && event.target !== input) choose(active);
		else if (event.key === 'Enter') {
			if (dirty) apply();
			else if (active < matches.length) {
				const final = new Map(current);
				const key = matches[active].key;
				final.set(key, nextState(initial.get(key) ?? 'none', current.get(key) ?? 'none'));
				apply(final);
			} else if (canCreate) void create();
		} else return;
		event.preventDefault();
	}
</script>

<dialog
	bind:this={dialog}
	aria-labelledby="label-picker-title"
	class="picker"
	onclose={close}
	onclick={(e) => {
		if (e.target === dialog) close();
	}}
	{onkeydown}
>
	<header>
		<h2 id="label-picker-title">Label as</h2>
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
			placeholder="Find or create a label"
			aria-label="Label name"
			role="combobox"
			aria-expanded="true"
			aria-controls="label-picker-list"
			aria-activedescendant={optionCount > 0 ? `pick-${active}` : undefined}
			autocomplete="off"
		/>
	</div>
	<ul id="label-picker-list" role="listbox" aria-label="Labels" aria-multiselectable="true">
		{#each matches as label, i (label.key)}
			{@const state = current.get(label.key) ?? 'none'}
			<li
				id="pick-{i}"
				role="option"
				aria-selected={state === 'all'}
				aria-checked={state === 'all' ? 'true' : state === 'some' ? 'mixed' : 'false'}
				class:active={i === active}
				onmousedown={(e) => e.preventDefault()}
				onclick={() => {
					active = i;
					toggle(label.key);
				}}
				onkeydown={() => {}}
			>
				<span class="box {state}" aria-hidden="true">
					{#if state === 'all'}<Icon name="check" size={14} />{:else if state === 'some'}<span
							class="dash"
						></span>{/if}
				</span>
				<span class="dot" style:--c={labelColor(label.key, label.color)}></span>
				<span class="name">{label.name}</span>
				{#if state !== (initial.get(label.key) ?? 'none')}<span class="changed">changed</span>{/if}
			</li>
		{/each}
		{#if canCreate}
			<li
				id="pick-{matches.length}"
				role="option"
				aria-selected="false"
				class="create"
				class:active={active === matches.length}
				onmousedown={(e) => e.preventDefault()}
				onclick={() => void create()}
				onkeydown={() => {}}
			>
				<span class="plus" aria-hidden="true"><Icon name="plus" size={16} /></span>
				<span class="name">Create “{filter.trim()}”</span>
			</li>
		{:else if matches.length === 0}
			<li class="none" role="presentation">No labels yet</li>
		{/if}
	</ul>
	{#if error}<p class="error" role="alert">{error}</p>{/if}
	<footer>
		<span class="hint"><kbd>Space</kbd> toggle · <kbd>Enter</kbd> apply</span>
		<button class="btn" onclick={close}>Cancel</button>
		<button class="btn primary" onclick={() => apply()} disabled={!dirty || busy}>Apply</button>
	</footer>
</dialog>

<style>
	.picker {
		width: min(400px, calc(100vw - 32px));
		margin-top: 12vh;
		padding: 0;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	.picker[open] {
		animation: rise 140ms var(--ease);
	}

	@keyframes rise {
		from {
			opacity: 0;
			transform: translateY(6px) scale(0.985);
		}
	}

	.picker::backdrop {
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
		gap: 10px;
		height: 38px;
		padding: 0 10px;
		border-radius: 8px;
		cursor: pointer;
		user-select: none;
	}

	li.active {
		background: var(--hover);
		box-shadow: inset 2px 0 0 var(--accent);
	}

	.box {
		display: grid;
		place-items: center;
		width: 18px;
		height: 18px;
		border: 1.5px solid var(--line-strong);
		border-radius: 5px;
		color: var(--accent-ink);
		transition:
			background 90ms var(--ease),
			border-color 90ms var(--ease);
	}

	.box.all,
	.box.some {
		border-color: var(--accent);
		background: var(--accent);
	}

	.dash {
		width: 8px;
		height: 2px;
		border-radius: 1px;
		background: var(--accent-ink);
	}

	.dot {
		flex: none;
		width: 14px;
		height: 10px;
		clip-path: polygon(4px 0, 100% 0, 100% 100%, 4px 100%, 0 50%);
		border-radius: 0 3px 3px 0;
		background: var(--c);
	}

	.name {
		flex: 1;
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.changed {
		color: var(--accent);
		font-size: 11px;
		font-weight: 600;
	}

	.create {
		color: var(--accent);
		font-weight: 560;
	}

	.plus {
		display: grid;
		place-items: center;
		width: 18px;
	}

	.none {
		color: var(--muted);
		cursor: default;
	}

	.error {
		margin: 8px 14px 0;
		padding: 8px 10px;
		border-radius: 8px;
		background: var(--danger-soft);
		color: var(--danger);
	}

	footer {
		display: flex;
		align-items: center;
		gap: 8px;
		margin-top: 8px;
		padding: 12px 14px;
		border-top: 1px solid var(--line);
	}

	.hint {
		display: flex;
		align-items: center;
		gap: 5px;
		flex: 1;
		color: var(--muted);
		font-size: 12px;
	}

	.hint kbd {
		height: 1.6em;
		font-size: 11px;
	}

	@media (max-width: 560px) {
		.hint {
			display: none;
		}

		footer {
			justify-content: flex-end;
		}
	}
</style>
