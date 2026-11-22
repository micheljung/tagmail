<script lang="ts">
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import VirtualList from '../list/VirtualList.svelte';
	import { shortcuts } from '../shortcuts';
	import { nav, type ThreadList } from '../stores';
	import ThreadRow from './ThreadRow.svelte';

	let {
		list,
		label,
		emptyTitle,
		emptyHint,
		hiddenLabel
	}: {
		list: ThreadList;
		label: string;
		emptyTitle: string;
		emptyHint: string;
		hiddenLabel?: string;
	} = $props();

	const COMPACT_QUERY = '(max-width: 720px)';
	const ROW_HEIGHT = 40;
	const ROW_HEIGHT_COMPACT = 76;

	let compact = $state(false);

	onMount(() => {
		const mq = matchMedia(COMPACT_QUERY);
		compact = mq.matches;
		const onChange = () => (compact = mq.matches);
		mq.addEventListener('change', onChange);
		list.attached = true;
		const pop = shortcuts.push((action) => {
			switch (action) {
				case 'next':
					list.move(1);
					return true;
				case 'previous':
					list.move(-1);
					return true;
				case 'open': {
					if (list.selectedId === null) return false;
					open(list.selectedId);
					return true;
				}
				default:
					return false;
			}
		});
		return () => {
			mq.removeEventListener('change', onChange);
			list.attached = false;
			pop();
		};
	});

	function open(id: number) {
		void goto(`/thread/${id}`);
	}
</script>

<section class="list" aria-label={label}>
	{#if list.status === 'loading'}
		<div class="state" role="status">
			<span class="spinner" aria-hidden="true"></span>
			<span>Loading conversations</span>
		</div>
	{:else if list.status === 'error'}
		<div class="state" role="alert">
			<strong>Couldn't load this list</strong>
			<span class="muted">{list.error}</span>
			<button class="btn" onclick={() => list.load()}>Try again</button>
		</div>
	{:else if list.count === 0}
		<div class="state empty">
			<div class="mark" aria-hidden="true"></div>
			<strong>{emptyTitle}</strong>
			<span class="muted">{emptyHint}</span>
		</div>
	{:else}
		<div class="bar">
			<span class="total" aria-live="polite">
				{list.count.toLocaleString()} conversation{list.count === 1 ? '' : 's'}
			</span>
			{#if list.error}<span class="warn" role="status">{list.error}</span>{/if}
		</div>
		<div class="viewport">
			{#key compact}
				<VirtualList
					count={list.count}
					rowHeight={compact ? ROW_HEIGHT_COMPACT : ROW_HEIGHT}
					version={list.indexVersion}
					activeIndex={list.activeIndex}
					{label}
					keyAt={(i) => list.idAt(i)}
					initialOffset={list.scrollOffset}
					autofocus
					onoffset={(v) => (list.scrollOffset = v)}
					onrange={(range, direction) => {
						list.noteScrollDirection(direction);
						list.ensureWindow(range);
					}}
					onselect={(i) => list.select(i)}
					onactivate={(i) => {
						const id = list.idAt(i);
						if (id !== undefined) open(id);
					}}
				>
					{#snippet row(index, active)}
						{@const id = list.idAt(index)}
						<ThreadRow
							row={id === undefined ? undefined : list.rows.get(id)}
							{active}
							labelName={(key) => nav.labelName(key)}
							{hiddenLabel}
						/>
					{/snippet}
				</VirtualList>
			{/key}
		</div>
	{/if}
</section>

<style>
	.list {
		display: flex;
		flex-direction: column;
		height: 100%;
		min-height: 0;
	}

	.bar {
		display: flex;
		align-items: center;
		gap: 12px;
		height: 36px;
		padding: 0 18px;
		border-bottom: 1px solid var(--line);
		color: var(--muted);
		font-size: 12px;
	}

	.total {
		font-family: var(--font-mono);
		font-variant-numeric: tabular-nums;
	}

	.warn {
		color: var(--danger);
	}

	.viewport {
		flex: 1;
		min-height: 0;
	}

	.state {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: 10px;
		height: 100%;
		padding: 32px;
		text-align: center;
	}

	.state strong {
		font-size: 16px;
	}

	.muted {
		max-width: 44ch;
		color: var(--muted);
	}

	.mark {
		width: 56px;
		height: 40px;
		margin-bottom: 6px;
		clip-path: polygon(14px 0, 100% 0, 100% 100%, 14px 100%, 0 50%);
		border-radius: 0 8px 8px 0;
		background: var(--accent-soft);
	}

	.spinner {
		width: 22px;
		height: 22px;
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
</style>
