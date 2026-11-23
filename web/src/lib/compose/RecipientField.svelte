<script lang="ts">
	import Icon from '../ui/Icon.svelte';
	import { formatRecipient, type Recipient } from './addresses';

	let {
		label,
		recipients,
		onadd,
		onremove,
		children
	}: {
		label: string;
		recipients: Recipient[];
		onadd: (text: string) => void;
		onremove: (index: number) => void;
		children?: import('svelte').Snippet;
	} = $props();

	let text = $state('');
	let input: HTMLInputElement;

	export function focus(): void {
		input.focus();
	}

	function commit() {
		if (!text.trim()) return;
		onadd(text);
		text = '';
	}

	function onkeydown(event: KeyboardEvent) {
		if (event.key === 'Enter' || event.key === ',' || event.key === ';') {
			if (!text.trim()) return;
			event.preventDefault();
			commit();
		} else if (event.key === 'Backspace' && text === '' && recipients.length > 0) {
			onremove(recipients.length - 1);
		}
	}

	function onpaste(event: ClipboardEvent) {
		const pasted = event.clipboardData?.getData('text/plain') ?? '';
		if (!/[,;\n]/.test(pasted)) return;
		event.preventDefault();
		onadd(text + pasted);
		text = '';
	}
</script>

<div class="field" role="group" aria-label={label}>
	<span class="label">{label}</span>
	<ul class="chips">
		{#each recipients as r, i (r.email + i)}
			<li class="chip" class:bad={!r.valid} title={r.valid ? r.email : 'Not a valid address'}>
				<span class="name">{r.name || r.email}</span>
				{#if !r.valid}<span class="sr-only">(not a valid address)</span>{/if}
				<button type="button" aria-label="Remove {formatRecipient(r)}" onclick={() => onremove(i)}>
					<Icon name="close" size={13} />
				</button>
			</li>
		{/each}
		<li class="grow">
			<input
				bind:this={input}
				bind:value={text}
				type="text"
				inputmode="email"
				autocomplete="off"
				spellcheck="false"
				aria-label="{label} recipients"
				{onkeydown}
				{onpaste}
				onblur={commit}
			/>
		</li>
	</ul>
	{@render children?.()}
</div>

<style>
	.field {
		display: flex;
		align-items: flex-start;
		gap: 8px;
		min-height: 40px;
		padding: 4px 12px 4px 16px;
		border-bottom: 1px solid var(--line);
	}

	.label {
		flex: none;
		width: 30px;
		padding-top: 8px;
		color: var(--muted);
		font-size: 13px;
	}

	.chips {
		display: flex;
		flex: 1;
		flex-wrap: wrap;
		align-items: center;
		gap: 4px;
		min-width: 0;
		margin: 0;
		padding: 2px 0;
		list-style: none;
	}

	.chip {
		display: inline-flex;
		align-items: center;
		gap: 2px;
		max-width: 100%;
		height: 26px;
		padding: 0 2px 0 10px;
		border: 1px solid var(--line-strong);
		border-radius: 999px;
		background: var(--surface-2);
		font-size: 13px;
		animation: pop 120ms var(--ease);
	}

	@keyframes pop {
		from {
			transform: scale(0.92);
			opacity: 0;
		}
	}

	.chip.bad {
		border-color: var(--danger);
		background: var(--danger-soft);
		color: var(--danger);
		text-decoration: underline wavy;
		text-decoration-thickness: 1px;
		text-underline-offset: 3px;
	}

	.name {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.chip button {
		display: grid;
		place-items: center;
		width: 22px;
		height: 22px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: inherit;
		opacity: 0.7;
		cursor: pointer;
	}

	.chip button:hover {
		background: var(--hover);
		opacity: 1;
	}

	.grow {
		flex: 1;
		min-width: 120px;
	}

	input {
		width: 100%;
		height: 30px;
		border: 0;
		outline: none;
		background: transparent;
	}
</style>
