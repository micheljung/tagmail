<script lang="ts">
	import { goto } from '$app/navigation';
	import Icon from './Icon.svelte';

	let { query = '' }: { query?: string } = $props();

	const OPERATORS: { token: string; hint: string }[] = [
		{ token: 'from:', hint: 'sender' },
		{ token: 'to:', hint: 'recipient' },
		{ token: 'subject:', hint: 'words in the subject' },
		{ token: 'label:', hint: 'has a label' },
		{ token: 'in:', hint: 'inbox, sent, trash, anywhere' },
		{ token: 'is:unread', hint: 'unread only' },
		{ token: 'is:starred', hint: 'starred only' },
		{ token: 'has:attachment', hint: 'with files' },
		{ token: 'after:', hint: 'yyyy/mm/dd' },
		{ token: 'before:', hint: 'yyyy/mm/dd' },
		{ token: '"…"', hint: 'exact phrase' },
		{ token: '-', hint: 'exclude a term' }
	];

	let value = $derived(query);
	let open = $state(false);
	let input: HTMLInputElement;
	let form: HTMLFormElement;

	export function focus(): void {
		input.focus();
		input.select();
	}

	function submit(event: SubmitEvent) {
		event.preventDefault();
		const q = value.trim();
		if (!q) return;
		open = false;
		input.blur();
		void goto(`/search?q=${encodeURIComponent(q)}`);
	}

	function insert(token: string) {
		const text = token === '"…"' ? '""' : token;
		const sep = value && !value.endsWith(' ') ? ' ' : '';
		value = value + sep + text;
		input.focus();
		const caret = token === '"…"' ? value.length - 1 : value.length;
		requestAnimationFrame(() => input.setSelectionRange(caret, caret));
	}

	function onfocusout(event: FocusEvent) {
		if (!form.contains(event.relatedTarget as Node | null)) open = false;
	}

	function onkeydown(event: KeyboardEvent) {
		if (event.key === 'Escape') {
			open = false;
			input.blur();
		}
	}
</script>

<form class="search" class:open role="search" bind:this={form} onsubmit={submit} {onfocusout}>
	<label class="field">
		<Icon name="search" size={19} />
		<span class="sr-only">Search mail</span>
		<input
			bind:this={input}
			bind:value
			type="search"
			placeholder="Search mail"
			autocomplete="off"
			spellcheck="false"
			aria-describedby="search-hints"
			onfocus={() => (open = true)}
			{onkeydown}
		/>
		<kbd class="slash" aria-hidden="true">/</kbd>
	</label>
	<div class="hints" id="search-hints" hidden={!open}>
		<p class="title">Narrow it down</p>
		<ul>
			{#each OPERATORS as op (op.token)}
				<li>
					<button type="button" onclick={() => insert(op.token)}>
						<code>{op.token}</code><span>{op.hint}</span>
					</button>
				</li>
			{/each}
		</ul>
	</div>
</form>

<style>
	.search {
		position: relative;
		flex: 1;
		max-width: 720px;
	}

	.field {
		display: flex;
		align-items: center;
		gap: 10px;
		height: 46px;
		padding: 0 14px 0 16px;
		border: 1px solid transparent;
		border-radius: 14px;
		background: var(--surface);
		color: var(--muted);
		transition:
			box-shadow 140ms var(--ease),
			border-radius 140ms var(--ease);
	}

	.open .field {
		border-color: var(--line);
		border-radius: 14px 14px 0 0;
		box-shadow: var(--shadow);
	}

	input {
		flex: 1;
		min-width: 0;
		height: 100%;
		border: 0;
		outline: none;
		background: transparent;
		color: var(--ink);
		font-size: 15px;
	}

	input::-webkit-search-cancel-button {
		cursor: pointer;
	}

	.slash {
		opacity: 0.8;
	}

	.open .slash {
		display: none;
	}

	.hints {
		position: absolute;
		left: 0;
		right: 0;
		top: 100%;
		z-index: 30;
		padding: 10px 10px 12px;
		border: 1px solid var(--line);
		border-top: 0;
		border-radius: 0 0 14px 14px;
		background: var(--surface-raised);
		box-shadow: var(--shadow-lg);
	}

	.title {
		margin: 2px 8px 8px;
		color: var(--muted);
		font-size: 11px;
		font-weight: 650;
		letter-spacing: 0.08em;
		text-transform: uppercase;
	}

	ul {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(200px, 1fr));
		gap: 2px;
		margin: 0;
		padding: 0;
		list-style: none;
	}

	li button {
		display: flex;
		align-items: baseline;
		gap: 10px;
		width: 100%;
		padding: 7px 8px;
		border: 0;
		border-radius: 8px;
		background: none;
		text-align: left;
		cursor: pointer;
	}

	li button:hover,
	li button:focus-visible {
		background: var(--hover);
	}

	code {
		font-family: var(--font-mono);
		font-size: 12.5px;
		color: var(--accent);
	}

	li span {
		color: var(--muted);
		font-size: 12.5px;
	}

	@media (max-width: 720px) {
		.field {
			height: 42px;
		}

		.slash {
			display: none;
		}

		ul {
			grid-template-columns: 1fr 1fr;
		}
	}
</style>
