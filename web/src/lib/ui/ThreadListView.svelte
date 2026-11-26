<script lang="ts">
	import { onMount } from 'svelte';
	import VirtualList from '../list/VirtualList.svelte';
	import { sectionLayout } from '../list/sections';
	import { act, inTrash, runOp, type ActionTarget } from '../ops/actions.svelte';
	import { SWIPE_OPS, type SwipeAllowed } from '../list/swipe';
	import { shortcuts, type ShortcutAction } from '../shortcuts';
	import { focusInListPane, listOwnsAct } from '../readingPane';
	import { accounts, nav, ops, prefs, type ThreadList } from '../stores';
	import type { ThreadRowDto } from '../transport';
	import { PARTIAL_TEXT } from '../desktop/desktop.svelte';
	import { viewScope } from '../views';
	import Icon, { type IconName } from './Icon.svelte';
	import { navigate } from './navigate';
	import { rowHeightFor } from '../prefs';
	import ThreadRow from './ThreadRow.svelte';

	let {
		list,
		label,
		emptyTitle,
		emptyHint,
		hiddenLabel,
		deferToThread = false,
		routeId = null
	}: {
		list: ThreadList;
		label: string;
		emptyTitle: string;
		emptyHint: string;
		hiddenLabel?: string;
		/** Shown next to an open conversation (`/thread/*`), which then owns j/k and its own acts. */
		deferToThread?: boolean;
		/** The open conversation's id, if any. */
		routeId?: number | null;
	} = $props();

	const COMPACT_QUERY = '(max-width: 720px)';
	const COARSE_QUERY = '(pointer: coarse)';

	let compact = $state(false);
	let coarse = $state(false);
	const rowHeight = $derived(rowHeightFor(prefs.density, { coarse, phone: compact }));

	const TOOLBAR: { action: ShortcutAction; icon: IconName; label: string; key: string }[] = [
		{ action: 'archive', icon: 'archive', label: 'Archive', key: 'e' },
		{ action: 'delete', icon: 'trash', label: 'Delete', key: '#' },
		{ action: 'markRead', icon: 'read', label: 'Mark as read', key: 'Shift+I' },
		{ action: 'markUnread', icon: 'unread', label: 'Mark as unread', key: 'Shift+U' },
		{ action: 'star', icon: 'star', label: 'Star', key: 's' },
		{ action: 'label', icon: 'tag', label: 'Label as', key: 'l' },
		{ action: 'move', icon: 'folder', label: 'Move to', key: 'v' }
	];

	/** Rows of a list that mixes accounts say which account they belong to. */
	const markers = $derived(
		accounts.multi && viewScope(list.view).account === null && !list.view.startsWith('mailbox:')
	);
	/** Unread-first inboxes show section headers between the rows. */
	const layout = $derived(list.sectioned ? sectionLayout(list.count, list.split) : undefined);
	const checkedCount = $derived(list.checked.size);
	const allState = $derived(
		checkedCount === 0 ? 'false' : checkedCount >= visibleCount() ? 'true' : 'mixed'
	);

	function visibleCount(): number {
		return Math.max(1, list.window.last - list.window.first + 1);
	}

	function target(): ActionTarget {
		const threadIds = list.targets();
		const rows = threadIds
			.map((id) => ops.project(list.rows.peek(id)))
			.filter((r): r is ThreadRowDto => r !== undefined);
		return { threadIds, rows, view: list.view };
	}

	/** Swipe never deletes for good, and a ticked selection is acted on from the toolbar. */
	const swipeAllowed: SwipeAllowed = $derived(
		checkedCount > 0 ? { left: false, right: false } : { left: true, right: !inTrash(list.view) }
	);

	function run(action: ShortcutAction): boolean {
		return act(action, target());
	}

	/** Takes focus on mount; next to a reading pane only when nothing else holds it. */
	function shouldAutofocus(): boolean {
		// A density or layout change from the quick settings remounts the list; focus stays there.
		if (document.activeElement?.closest('[data-quick-settings]')) return false;
		if (!prefs.split) return true;
		if (deferToThread) return false;
		const active = document.activeElement;
		return !active || active === document.body || focusInListPane();
	}

	onMount(() => {
		const mq = matchMedia(COMPACT_QUERY);
		const coarseMq = matchMedia(COARSE_QUERY);
		const onChange = () => {
			compact = mq.matches;
			coarse = coarseMq.matches;
		};
		onChange();
		mq.addEventListener('change', onChange);
		coarseMq.addEventListener('change', onChange);
		const detach = list.attach();
		const pop = shortcuts.push((action) => {
			switch (action) {
				case 'next':
					if (deferToThread) return false;
					list.move(1);
					return true;
				case 'previous':
					if (deferToThread) return false;
					list.move(-1);
					return true;
				case 'open': {
					if (list.selectedId === null) return false;
					if (deferToThread && list.selectedId === routeId) return false;
					open(list.selectedId);
					return true;
				}
				case 'select':
					if (list.activeIndex === -1) return false;
					list.toggleChecked(list.activeIndex);
					return true;
				default:
					if (
						deferToThread &&
						!listOwnsAct({
							checked: list.checked.size,
							selectedId: list.selectedId,
							routeId,
							focusInList: focusInListPane()
						})
					) {
						return false;
					}
					return run(action);
			}
		});
		return () => {
			mq.removeEventListener('change', onChange);
			coarseMq.removeEventListener('change', onChange);
			detach();
			pop();
		};
	});

	function open(id: number) {
		void navigate(`/thread/${id}`);
	}
</script>

<section class="list" aria-label={label}>
	{#if list.partial}
		<p class="partial" role="status" data-partial>
			<Icon name="alert" size={16} />{PARTIAL_TEXT}
		</p>
	{/if}
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
		<div class="bar" role="toolbar" aria-label="Conversation actions">
			<button
				class="all"
				role="checkbox"
				aria-checked={allState}
				aria-label="Select all conversations on screen"
				title="Select (x)"
				onclick={() => list.setAllChecked(checkedCount === 0)}
			>
				<span class="box {allState}" aria-hidden="true">
					{#if allState === 'true'}<Icon
							name="check"
							size={13}
						/>{:else if allState === 'mixed'}<span class="dash"></span>{/if}
				</span>
			</button>
			{#if checkedCount > 0}
				<div class="actions">
					{#each TOOLBAR as item (item.action)}
						<button
							class="act"
							aria-label={item.label}
							title="{item.label} ({item.key})"
							onclick={() => run(item.action)}
						>
							<Icon name={item.icon} size={18} />
						</button>
					{/each}
				</div>
				<span class="total" aria-live="polite">{checkedCount.toLocaleString()} selected</span>
			{:else}
				<span class="total" aria-live="polite">
					{list.count.toLocaleString()} conversation{list.count === 1 ? '' : 's'}
				</span>
			{/if}
			{#if list.error}<span class="warn" role="status">{list.error}</span>{/if}
		</div>
		<div class="viewport">
			{#key rowHeight}
				<VirtualList
					count={list.count}
					{rowHeight}
					version={list.indexVersion}
					activeIndex={list.activeIndex}
					{label}
					keyAt={(i) => list.idAt(i)}
					initialOffset={list.scrollRow * rowHeight}
					autofocus={shouldAutofocus()}
					onoffset={(rows) => (list.scrollRow = rows)}
					onrange={(range, direction) => {
						list.noteScrollDirection(direction);
						list.ensureWindow(range);
					}}
					onselect={(i) => list.select(i)}
					onactivate={(i) => {
						const id = list.idAt(i);
						if (id !== undefined) open(id);
					}}
					{layout}
				>
					{#snippet header(section)}
						<h2 class="section">{section === 'unread' ? 'Unread' : 'Everything else'}</h2>
					{/snippet}
					{#snippet row(index, active)}
						{@const id = list.idAt(index)}
						{@const row = id === undefined ? undefined : ops.project(list.rows.get(id))}
						<ThreadRow
							{row}
							{active}
							checked={id !== undefined && list.checked.has(id)}
							labelName={(key) => nav.labelName(key)}
							labelColor={(key) => nav.labelColor(key)}
							{hiddenLabel}
							accountName={markers && row ? accounts.name(row.accountId) : undefined}
							swipe={swipeAllowed}
							onswipe={(direction) => {
								if (id === undefined || !row) return;
								runOp(
									{ kind: SWIPE_OPS[direction] },
									{ threadIds: [id], rows: [row], view: list.view }
								);
							}}
							oncheck={(range) => list.toggleChecked(index, range)}
							onstar={() => {
								if (id === undefined || !row) return;
								act('star', { threadIds: [id], rows: [row], view: list.view });
							}}
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
		height: 40px;
		padding: 0 18px;
		border-bottom: 1px solid var(--line);
		color: var(--muted);
		font-size: 12px;
	}

	.bar {
		padding-left: 8px;
	}

	.all,
	.act {
		display: grid;
		place-items: center;
		width: 32px;
		height: 32px;
		border: 0;
		border-radius: 50%;
		background: none;
		color: var(--ink-2);
		cursor: pointer;
	}

	.all:hover,
	.act:hover {
		background: var(--hover);
		color: var(--ink);
	}

	.box {
		display: grid;
		place-items: center;
		width: 16px;
		height: 16px;
		border: 1.5px solid var(--line-strong);
		border-radius: 4px;
		color: var(--accent-ink);
	}

	.box.true,
	.box.mixed {
		border-color: var(--accent);
		background: var(--accent);
	}

	.dash {
		width: 8px;
		height: 2px;
		border-radius: 1px;
		background: var(--accent-ink);
	}

	.actions {
		display: flex;
		gap: 2px;
		padding-right: 8px;
		margin-right: 4px;
		border-right: 1px solid var(--line);
		animation: fade 120ms var(--ease);
	}

	@keyframes fade {
		from {
			opacity: 0;
			transform: translateX(-4px);
		}
	}

	@media (pointer: coarse), (max-width: 720px) {
		.bar {
			height: 48px;
			padding-left: 0;
		}

		.all,
		.act {
			width: 44px;
			height: 44px;
		}

		.actions {
			gap: 0;
			padding-right: 0;
		}
	}

	.total {
		font-family: var(--font-mono);
		font-variant-numeric: tabular-nums;
	}

	.warn {
		color: var(--danger);
	}

	.partial {
		display: flex;
		align-items: center;
		gap: 8px;
		margin: 0;
		padding: 8px 16px;
		background: var(--surface-2);
		color: var(--muted);
		font-size: 13px;
	}

	.viewport {
		flex: 1;
		min-height: 0;
	}

	.section {
		display: flex;
		align-items: flex-end;
		height: 100%;
		margin: 0;
		padding: 0 18px 6px;
		border-bottom: 1px solid var(--line);
		color: var(--muted);
		font-size: 12px;
		font-weight: 600;
		letter-spacing: 0.02em;
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
