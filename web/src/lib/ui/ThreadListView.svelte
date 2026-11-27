<script lang="ts">
	import { afterNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount, tick, untrack } from 'svelte';
	import VirtualList from '../list/VirtualList.svelte';
	import { sectionLayout } from '../list/sections';
	import { act, inTrash, runOp, type ActionTarget } from '../ops/actions.svelte';
	import { SWIPE_OPS, type SwipeAllowed } from '../list/swipe';
	import { shortcuts, type ShortcutAction } from '../shortcuts';
	import { focusInListPane, listOwnsAct } from '../readingPane';
	import { accounts, nav, ops, prefs, type ThreadList } from '../stores';
	import type { RefreshState } from '../stores/threadList.svelte';
	import { isDesktop, type ThreadRowDto } from '../transport';
	import { PARTIAL_TEXT } from '../desktop/desktop.svelte';
	import { viewScope } from '../views';
	import Icon, { type IconName } from './Icon.svelte';
	import { selectionBar } from './selectionBar.svelte';
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

	const REFRESH_TEXT: Partial<Record<RefreshState, string>> = {
		done: 'Up to date',
		failed: "Couldn't refresh",
		offline: "Offline, couldn't refresh"
	};
	const refreshText = $derived(REFRESH_TEXT[list.refreshing] ?? '');

	/** Phones and touch screens: a hold selects, taps then toggle, Back clears. */
	const touchMode = $derived(compact || coarse);
	/** The selection action bar replaces the app header; phones only. */
	const cab = $derived(compact && checkedCount > 0);
	const anyUnread = $derived(cab && target().rows.some((r) => r.unreadCount > 0));

	let sectionEl: HTMLElement | undefined = $state();
	let moreButton: HTMLButtonElement | undefined = $state();
	let moreMenu: HTMLDivElement | undefined = $state();
	let moreOpen = $state(false);
	/** The thread ticked or unticked last, for focus on exit. */
	let lastToggled: number | undefined;
	/** Whether this list pushed the history entry Back pops to leave selection. */
	let selectionEntryPushed = false;

	$effect(() => {
		selectionBar.active = cab;
		return () => {
			selectionBar.active = false;
		};
	});

	$effect(() => {
		if (!cab) moreOpen = false;
	});

	// Selection starts: push the entry Back pops. It reaches 0: leave it.
	$effect(() => {
		const n = checkedCount;
		untrack(() => {
			if (n > 0 && touchMode && !selectionEntryPushed) {
				selectionEntryPushed = true;
				void goto(page.url.href, { shallow: true, state: { ...page.state, selection: true } });
			} else if (n === 0 && selectionEntryPushed) exitSelection();
		});
	});

	// Back popped the selection entry; the browser already left it.
	$effect(() => {
		const inSelection = page.state.selection;
		untrack(() => {
			if (inSelection || !selectionEntryPushed) return;
			selectionEntryPushed = false;
			list.setAllChecked(false);
			restoreFocus();
		});
	});

	// Coming back to an entry a selection left behind: make it a plain list.
	afterNavigate(() => {
		void tick().then(() => {
			if (page.state.selection && list.checked.size === 0 && !selectionEntryPushed) {
				const state = { ...page.state };
				delete state.selection;
				void goto(page.url.href, { shallow: true, replace: true, state });
			}
		});
	});

	/** Leaves selection mode; safe to call more than once. */
	function exitSelection() {
		const was = selectionEntryPushed || list.checked.size > 0;
		list.setAllChecked(false);
		if (selectionEntryPushed) {
			// Cleared before back(): popstate comes later and must not go back again.
			selectionEntryPushed = false;
			history.back();
		}
		if (was) restoreFocus();
	}

	function restoreFocus() {
		moreOpen = false;
		void tick().then(() => {
			const at = lastToggled === undefined ? -1 : list.index.indexOf(lastToggled);
			if (at !== -1) list.select(at);
			sectionEl?.querySelector<HTMLElement>('.scroller')?.focus({ preventScroll: true });
		});
	}

	function toggle(index: number) {
		lastToggled = list.idAt(index);
		list.toggleChecked(index);
	}

	/** Bar actions that finish at once end the selection. */
	function cabRun(action: ShortcutAction) {
		moreOpen = false;
		if (run(action)) exitSelection();
	}

	/** Picker actions end the selection only when the picker is confirmed. */
	function cabPick(action: 'label' | 'move') {
		moreOpen = false;
		act(action, { ...target(), onapply: exitSelection });
	}

	async function openMore() {
		moreOpen = true;
		await tick();
		moreMenu?.querySelector<HTMLElement>('[role=menuitem]')?.focus();
	}

	function closeMore() {
		moreOpen = false;
		moreButton?.focus();
	}

	function onMoreKey(e: KeyboardEvent) {
		const items = [...(moreMenu?.querySelectorAll<HTMLElement>('[role=menuitem]') ?? [])];
		const at = items.indexOf(document.activeElement as HTMLElement);
		if (e.key === 'Escape') closeMore();
		else if (e.key === 'ArrowDown') items[(at + 1) % items.length]?.focus();
		else if (e.key === 'ArrowUp') items[(at - 1 + items.length) % items.length]?.focus();
		else if (e.key === 'Tab') moreOpen = false;
		else return;
		if (e.key !== 'Tab') {
			e.preventDefault();
			e.stopPropagation();
		}
	}

	/** Shift+F10 or the ContextMenu key: the keyboard way into selection on phones. */
	function onListKey(e: KeyboardEvent) {
		if (!compact || list.activeIndex === -1) return;
		if (e.key !== 'ContextMenu' && !(e.shiftKey && e.key === 'F10')) return;
		e.preventDefault();
		const id = list.idAt(list.activeIndex);
		if (id !== undefined && !list.checked.has(id)) toggle(list.activeIndex);
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
			// Navigated away mid-selection: the selection does not come back with the list.
			if (selectionEntryPushed) {
				selectionEntryPushed = false;
				list.setAllChecked(false);
			}
		};
	});

	function open(id: number) {
		void navigate(`/thread/${id}`);
	}
</script>

<svelte:window
	onpointerdown={(e) => {
		if (moreOpen && !moreMenu?.parentElement?.contains(e.target as Node)) moreOpen = false;
	}}
/>

<section class="list" aria-label={label} bind:this={sectionEl}>
	<span class="sr-only" aria-live="polite">
		{compact && checkedCount > 0 ? `${checkedCount.toLocaleString()} selected` : ''}
	</span>
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
		{#if cab}
			<div class="cab" role="toolbar" aria-label="Selection actions">
				<button class="act" aria-label="Close selection" onclick={exitSelection}>
					<Icon name="close" size={20} />
				</button>
				<span class="total picked">{checkedCount.toLocaleString()} selected</span>
				<button class="act" aria-label="Archive" onclick={() => cabRun('archive')}>
					<Icon name="archive" size={20} />
				</button>
				<button class="act" aria-label="Delete" onclick={() => cabRun('delete')}>
					<Icon name="trash" size={20} />
				</button>
				{#if anyUnread}
					<button class="act" aria-label="Mark as read" onclick={() => cabRun('markRead')}>
						<Icon name="read" size={20} />
					</button>
				{:else}
					<button class="act" aria-label="Mark as unread" onclick={() => cabRun('markUnread')}>
						<Icon name="unread" size={20} />
					</button>
				{/if}
				<div class="more">
					<button
						class="act"
						aria-label="More"
						aria-haspopup="menu"
						aria-expanded={moreOpen}
						aria-controls="selection-more"
						bind:this={moreButton}
						onclick={() => (moreOpen ? closeMore() : void openMore())}
					>
						<svg width="20" height="20" viewBox="0 0 24 24" aria-hidden="true" fill="currentColor">
							<circle cx="12" cy="5.5" r="1.7" />
							<circle cx="12" cy="12" r="1.7" />
							<circle cx="12" cy="18.5" r="1.7" />
						</svg>
					</button>
					{#if moreOpen}
						<div
							class="menu"
							id="selection-more"
							role="menu"
							aria-label="More actions"
							tabindex="-1"
							bind:this={moreMenu}
							onkeydown={onMoreKey}
						>
							<button role="menuitem" onclick={() => cabPick('label')}>
								<Icon name="tag" size={18} />Label as
							</button>
							<button role="menuitem" onclick={() => cabPick('move')}>
								<Icon name="folder" size={18} />Move to
							</button>
							<button role="menuitem" onclick={() => cabRun('star')}>
								<Icon name="star" size={18} />Star
							</button>
							<button
								role="menuitem"
								onclick={() => {
									moreOpen = false;
									list.setAllChecked(true);
								}}
							>
								<Icon name="check" size={18} />Select all
							</button>
						</div>
					{/if}
				</div>
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
		{/if}
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<div class="viewport" onkeydown={onListKey}>
			<p class="refresh" class:shown={refreshText !== ''} role="status" aria-live="polite">
				{refreshText}
			</p>
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
					pullToRefresh={{
						enabled: touchMode && !isDesktop() && checkedCount === 0,
						busy: list.refreshing === 'busy',
						onrefresh: () => list.refresh()
					}}
					onoffset={(rows) => (list.scrollRow = rows)}
					onrange={(range, direction) => {
						list.noteScrollDirection(direction);
						list.ensureWindow(range);
					}}
					onselect={(i) => list.select(i)}
					onactivate={(i) => {
						// Touch selection mode: a tap toggles; desktop keeps opening the thread.
						if (touchMode && checkedCount > 0) return toggle(i);
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
							oncheck={(range) => {
								lastToggled = id;
								list.toggleChecked(index, range);
							}}
							onhold={touchMode
								? () => {
										if (id === undefined || list.checked.has(id)) return;
										list.select(index);
										toggle(index);
									}
								: undefined}
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

	/* Phone selection bar: takes the app header's place and height. */
	.cab {
		position: sticky;
		top: 0;
		z-index: 2;
		flex: none;
		display: flex;
		align-items: center;
		height: calc(64px + env(safe-area-inset-top));
		padding: env(safe-area-inset-top) 4px 0;
		background: var(--accent);
		color: var(--accent-ink);
	}

	.cab .act {
		width: 44px;
		height: 44px;
		color: inherit;
	}

	.cab .act:hover {
		background: color-mix(in oklab, var(--accent-ink) 14%, transparent);
		color: inherit;
	}

	.cab .picked {
		flex: 1;
		min-width: 0;
		padding: 0 8px;
		overflow: hidden;
		font-family: inherit;
		font-size: 17px;
		font-weight: 650;
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
		text-overflow: ellipsis;
	}

	.more {
		position: relative;
	}

	.menu {
		position: absolute;
		top: calc(100% + 4px);
		right: 0;
		z-index: 3;
		display: flex;
		flex-direction: column;
		min-width: 200px;
		padding: 6px 0;
		border: 1px solid var(--line);
		border-radius: var(--radius);
		background: var(--surface);
		box-shadow: var(--shadow-lg);
		color: var(--ink);
	}

	.menu button {
		display: flex;
		align-items: center;
		gap: 14px;
		min-height: 48px;
		padding: 0 18px;
		border: 0;
		background: none;
		color: inherit;
		font: inherit;
		font-size: 15px;
		text-align: left;
		cursor: pointer;
	}

	.menu button:hover,
	.menu button:focus-visible {
		background: var(--hover);
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
		position: relative;
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

	/* Pull-to-refresh outcome: an overlay, never in the scroller's flow. */
	.refresh {
		position: absolute;
		top: 12px;
		left: 50%;
		z-index: 2;
		margin: 0;
		padding: 8px 14px;
		border-radius: 999px;
		background: var(--ink);
		color: var(--surface);
		font-size: 13px;
		font-weight: 600;
		white-space: nowrap;
		pointer-events: none;
		opacity: 0;
		transform: translateX(-50%);
		transition: opacity 120ms linear;
	}

	.refresh.shown {
		opacity: 1;
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
