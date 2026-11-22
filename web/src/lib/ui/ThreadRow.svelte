<script lang="ts">
	import type { ThreadRowDto } from '@tagmail/api-types';
	import { listDate, senderLine } from '../format';
	import Icon from './Icon.svelte';
	import LabelChip from './LabelChip.svelte';

	let {
		row,
		active,
		labelName,
		hiddenLabel
	}: {
		row: ThreadRowDto | undefined;
		active: boolean;
		labelName: (key: string) => string;
		/** The label the list is filtered by; Gmail omits its chip. */
		hiddenLabel?: string;
	} = $props();

	const unread = $derived((row?.unreadCount ?? 0) > 0);
	const labels = $derived(row ? row.labels.filter((l) => l !== hiddenLabel) : []);
</script>

{#if row}
	<div class="row" class:unread class:active data-loaded="1">
		<span class="star" class:on={row.starred}>
			<Icon
				name="star"
				size={17}
				filled={row.starred}
				label={row.starred ? 'Starred' : undefined}
			/>
		</span>
		<span class="from">
			<span class="who">{senderLine(row.senders)}</span>
			{#if row.messageCount > 1}<span class="count">{row.messageCount}</span>{/if}
		</span>
		<span class="body">
			{#each labels as key (key)}<LabelChip labelKey={key} name={labelName(key)} />{/each}
			<span class="subject">{row.subject || '(no subject)'}</span>
			<span class="snippet">{row.snippet}</span>
		</span>
		<span class="meta">
			{#if row.hasAttachment}<Icon name="clip" size={15} label="Has attachment" />{/if}
			<time datetime={new Date(row.lastDate * 1000).toISOString()}>{listDate(row.lastDate)}</time>
		</span>
	</div>
{:else}
	<div class="row skeleton" aria-busy="true">
		<span class="star"></span>
		<span class="from"><span class="bar w60"></span></span>
		<span class="body"><span class="bar w80"></span></span>
		<span class="meta"><span class="bar w40"></span></span>
	</div>
{/if}

<style>
	.row {
		--row-bg: var(--surface-2);
		position: relative;
		display: grid;
		grid-template-columns: 34px minmax(120px, 200px) minmax(0, 1fr) auto;
		align-items: center;
		gap: 0 12px;
		height: 100%;
		padding: 0 16px 0 10px;
		border-bottom: 1px solid var(--line);
		background: var(--row-bg);
		color: var(--ink-2);
		transition: background 80ms linear;
	}

	.row.unread {
		--row-bg: var(--surface);
		color: var(--ink);
	}

	.row:hover {
		--row-bg: var(--hover);
		box-shadow:
			inset 1px 0 0 var(--line-strong),
			0 1px 3px rgb(0 0 0 / 0.08);
		z-index: 1;
	}

	.row.active {
		--row-bg: var(--select);
	}

	.row.active::before {
		content: '';
		position: absolute;
		left: 0;
		top: 0;
		bottom: 0;
		width: 3px;
		background: var(--accent);
	}

	.star {
		display: grid;
		place-items: center;
		color: var(--faint);
	}

	.star.on {
		color: var(--star);
	}

	.from {
		display: flex;
		align-items: baseline;
		gap: 6px;
		min-width: 0;
	}

	.who {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.unread .who,
	.unread .subject {
		font-weight: 680;
	}

	.count {
		flex: none;
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--muted);
	}

	.body {
		display: flex;
		align-items: center;
		gap: 6px;
		min-width: 0;
		overflow: hidden;
		white-space: nowrap;
	}

	.subject {
		flex: none;
		max-width: 70%;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.snippet {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		color: var(--muted);
	}

	.snippet::before {
		content: '— ';
		color: var(--faint);
	}

	.meta {
		display: flex;
		align-items: center;
		gap: 8px;
		color: var(--muted);
	}

	time {
		min-width: 64px;
		font-family: var(--font-mono);
		font-size: 12px;
		font-variant-numeric: tabular-nums;
		text-align: right;
	}

	.unread time {
		color: var(--ink);
		font-weight: 600;
	}

	.skeleton .bar {
		display: block;
		height: 9px;
		border-radius: 5px;
		background: linear-gradient(90deg, var(--line) 0%, var(--hover) 50%, var(--line) 100%);
		background-size: 200% 100%;
		animation: shimmer 1.2s linear infinite;
	}

	.w40 {
		width: 48px;
	}
	.w60 {
		width: 60%;
	}
	.w80 {
		width: 80%;
	}

	@keyframes shimmer {
		to {
			background-position: -200% 0;
		}
	}

	@media (max-width: 720px) {
		.row {
			grid-template-columns: minmax(0, 1fr) auto;
			grid-template-rows: auto auto auto;
			align-content: center;
			gap: 1px 10px;
			padding: 0 14px 0 16px;
		}

		.star {
			grid-column: 2;
			grid-row: 2 / span 2;
		}

		.from {
			grid-column: 1;
			grid-row: 1;
			font-size: 15px;
		}

		.meta {
			grid-column: 2;
			grid-row: 1;
		}

		.body {
			grid-column: 1;
			grid-row: 2 / span 2;
			display: grid;
			grid-template-columns: minmax(0, 1fr);
			gap: 1px;
		}

		.body :global(.chip) {
			display: none;
		}

		.subject {
			max-width: none;
		}

		.snippet::before {
			content: none;
		}

		.skeleton .body {
			display: block;
		}
	}
</style>
