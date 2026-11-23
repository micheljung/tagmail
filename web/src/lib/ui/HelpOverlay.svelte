<script lang="ts">
	import { HELP } from '../shortcuts';
	import Icon from './Icon.svelte';

	let { open = $bindable(false) }: { open?: boolean } = $props();

	let dialog: HTMLDialogElement;

	const GROUPS = ['Navigate', 'Act', 'Write'] as const;

	function joiner(action: string): string {
		if (action === 'open') return 'or';
		if (action === 'markRead' || action === 'markUnread') return '+';
		return 'then';
	}

	$effect(() => {
		if (open && !dialog.open) dialog.showModal();
		else if (!open && dialog.open) dialog.close();
	});
</script>

<dialog
	bind:this={dialog}
	aria-labelledby="help-title"
	onclose={() => (open = false)}
	onclick={(e) => {
		if (e.target === dialog) open = false;
	}}
>
	<header>
		<h2 id="help-title">Keyboard shortcuts</h2>
		<button class="close" onclick={() => (open = false)} aria-label="Close">
			<Icon name="close" />
		</button>
	</header>
	{#each GROUPS as group (group)}
		<h3>{group}</h3>
		<dl>
			{#each HELP.filter((h) => h.group === group) as item (item.action)}
				<div class="item">
					<dt>
						{#each item.keys as key, i (key)}
							{#if i > 0}<span class="then">{joiner(item.action)}</span>{/if}<kbd>
								{key}
							</kbd>
						{/each}
					</dt>
					<dd>{item.label}</dd>
				</div>
			{/each}
		</dl>
	{/each}
	<p class="foot">Shortcuts are off while you type in a field. <kbd>Esc</kbd> leaves it.</p>
</dialog>

<style>
	dialog {
		width: min(620px, calc(100vw - 32px));
		padding: 0;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	dialog::backdrop {
		background: rgb(8 10 14 / 0.45);
		backdrop-filter: blur(2px);
	}

	dialog[open] {
		animation: rise 160ms var(--ease);
	}

	@keyframes rise {
		from {
			opacity: 0;
			transform: translateY(8px) scale(0.98);
		}
	}

	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		padding: 18px 18px 6px 24px;
	}

	h2 {
		margin: 0;
		font-size: 18px;
		font-weight: 650;
	}

	.close {
		display: grid;
		place-items: center;
		width: 36px;
		height: 36px;
		border: 0;
		border-radius: 50%;
		background: none;
		cursor: pointer;
	}

	.close:hover {
		background: var(--hover);
	}

	dialog {
		max-height: calc(100dvh - 48px);
	}

	h3 {
		margin: 14px 24px 0;
		color: var(--muted);
		font-size: 11px;
		font-weight: 650;
		letter-spacing: 0.09em;
		text-transform: uppercase;
	}

	dl {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 0 24px;
		margin: 0;
		padding: 2px 24px 4px;
	}

	.item {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 12px;
		padding: 9px 0;
		border-bottom: 1px solid var(--line);
	}

	dt {
		display: flex;
		align-items: center;
		gap: 5px;
		order: 2;
	}

	dd {
		margin: 0;
		color: var(--ink-2);
	}

	.then {
		color: var(--faint);
		font-size: 11px;
	}

	.foot {
		margin: 0;
		padding: 14px 24px 20px;
		color: var(--muted);
		font-size: 12.5px;
	}

	@media (max-width: 560px) {
		dl {
			grid-template-columns: 1fr;
		}
	}
</style>
