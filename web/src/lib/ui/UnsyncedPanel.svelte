<script lang="ts">
	import { calendarNote } from '../calendar/opState';
	import { outbox } from '../compose/composer.svelte';
	import { ops } from '../stores';
	import Icon from './Icon.svelte';

	let { open = $bindable(false) }: { open?: boolean } = $props();

	let dialog: HTMLDialogElement;

	const items = $derived(ops.unsynced);
	const failedSends = $derived(outbox.failed);
	const failedOps = $derived(items.filter((o) => o.status === 'failed'));

	$effect(() => {
		if (open && !dialog.open) dialog.showModal();
		else if (!open && dialog.open) dialog.close();
	});

	function statusText(status: string): string {
		if (status === 'failed') return 'Failed';
		if (status === 'sending') return 'Sending';
		return 'Waiting for the server';
	}
</script>

<dialog
	bind:this={dialog}
	aria-labelledby="unsynced-title"
	onclose={() => (open = false)}
	onclick={(e) => {
		if (e.target === dialog) open = false;
	}}
>
	<header>
		<div>
			<h2 id="unsynced-title">Unsynced changes</h2>
			<p class="lead">Changes shown here haven't reached your mail server yet.</p>
		</div>
		<button class="close" onclick={() => (open = false)} aria-label="Close">
			<Icon name="close" />
		</button>
	</header>

	{#if items.length === 0 && failedSends.length === 0}
		<div class="empty">
			<span class="ok" aria-hidden="true"><Icon name="check" size={22} /></span>
			<strong>Everything is in sync</strong>
		</div>
	{:else}
		<ul class="list" aria-label="Unsynced changes">
			{#each items as op (op.opId)}
				{@const note = op.calendar ? calendarNote(op.status, op.calendar) : null}
				<li class={op.status} data-op={op.opId}>
					<span class="glyph" aria-hidden="true">
						{#if op.calendar}<Icon
								name="calendar"
								size={18}
							/>{:else if op.status === 'failed'}<Icon name="alert" size={18} />{:else}<span
								class="spin"
							></span>{/if}
					</span>
					<div class="what">
						<strong>{ops.describe(op)}</strong>
						<span>
							<span class="badge">{statusText(op.status)}</span>
							{#if note}<span class="err">{note.text}</span>{:else if op.error}<span class="err">
									{op.error}
								</span>{/if}
						</span>
					</div>
					{#if op.status === 'failed'}
						<button class="btn" onclick={() => void ops.retry(op)}>
							{note?.retryLabel ?? 'Retry'}
						</button>
					{/if}
					<button class="btn ghost" onclick={() => void ops.discard(op)}>Discard</button>
				</li>
			{/each}
			{#each failedSends as item (item.opId)}
				<li class="failed">
					<span class="glyph" aria-hidden="true"><Icon name="send" size={18} /></span>
					<div class="what">
						<strong>Send “{item.subject || '(no subject)'}”</strong>
						<span>
							<span class="badge">Failed</span>
							{#if item.error}<span class="err">{item.error}</span>{/if}
						</span>
					</div>
					<button class="btn" onclick={() => void outbox.retry(item)}>Retry</button>
					<button class="btn ghost" onclick={() => outbox.dismiss(item)}>Discard</button>
				</li>
			{/each}
		</ul>
		{#if failedOps.length > 1}
			<footer>
				<button class="btn primary" onclick={() => failedOps.forEach((op) => void ops.retry(op))}>
					<Icon name="sync" size={16} />Retry all
				</button>
			</footer>
		{/if}
	{/if}
</dialog>

<style>
	dialog {
		width: min(560px, calc(100vw - 32px));
		max-height: min(640px, calc(100dvh - 48px));
		padding: 0;
		border: 1px solid var(--line);
		border-radius: var(--radius-lg);
		background: var(--surface-raised);
		color: var(--ink);
		box-shadow: var(--shadow-lg);
	}

	dialog[open] {
		animation: rise 160ms var(--ease);
	}

	@keyframes rise {
		from {
			opacity: 0;
			transform: translateY(8px);
		}
	}

	dialog::backdrop {
		background: rgb(8 10 14 / 0.4);
	}

	header {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: 12px;
		padding: 20px 18px 12px 24px;
	}

	h2 {
		margin: 0;
		font-size: 18px;
		font-weight: 650;
	}

	.lead {
		margin: 4px 0 0;
		color: var(--muted);
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

	.list {
		margin: 0;
		padding: 0 16px 8px;
		list-style: none;
	}

	.list li {
		display: flex;
		align-items: center;
		gap: 12px;
		padding: 12px 10px;
		border-top: 1px solid var(--line);
	}

	.glyph {
		display: grid;
		place-items: center;
		flex: none;
		width: 34px;
		height: 34px;
		border-radius: 10px;
		background: var(--surface-2);
		color: var(--muted);
	}

	.failed .glyph {
		background: var(--danger-soft);
		color: var(--danger);
	}

	.what {
		display: grid;
		flex: 1;
		gap: 3px;
		min-width: 0;
	}

	.what strong {
		font-weight: 600;
	}

	.what > span {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		font-size: 12.5px;
	}

	.badge {
		padding: 2px 8px;
		border-radius: 999px;
		background: var(--surface-2);
		color: var(--muted);
		font-weight: 650;
	}

	.failed .badge {
		background: var(--danger-soft);
		color: var(--danger);
	}

	.err {
		color: var(--muted);
	}

	.btn {
		height: 32px;
	}

	.btn.ghost {
		border-color: transparent;
		background: none;
		color: var(--muted);
	}

	.btn.ghost:hover {
		background: var(--hover);
		color: var(--ink);
	}

	.spin {
		width: 14px;
		height: 14px;
		border: 2px solid var(--line-strong);
		border-top-color: var(--accent);
		border-radius: 50%;
		animation: spin 700ms linear infinite;
	}

	@keyframes spin {
		to {
			transform: rotate(1turn);
		}
	}

	.empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 10px;
		padding: 28px 24px 36px;
	}

	.ok {
		display: grid;
		place-items: center;
		width: 44px;
		height: 44px;
		border-radius: 50%;
		background: color-mix(in oklab, var(--ok) 16%, transparent);
		color: var(--ok);
	}

	footer {
		display: flex;
		justify-content: flex-end;
		padding: 4px 24px 18px;
	}

	@media (max-width: 560px) {
		.list li {
			flex-wrap: wrap;
		}
	}
</style>
