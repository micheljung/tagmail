<script lang="ts">
	import { onMount, type Snippet } from 'svelte';
	import {
		clampIndex,
		pageRows,
		physicalTop,
		revealTarget,
		rowOffset,
		spacerHeight,
		virtualTop,
		visibleRange,
		type ListGeometry,
		type Range
	} from './math';
	import { PULL_THRESHOLD_PX, PullTracker } from './pull';
	import type { Section, SectionLayout } from './sections';

	export interface PullToRefresh {
		/** Re-read on every move and on release; false disengages a pull at once. */
		enabled: boolean;
		busy: boolean;
		onrefresh: () => void;
	}

	interface Props {
		/** Threads; `activeIndex`, `onselect`, `onactivate` and `onrange` speak thread indexes. */
		count: number;
		rowHeight: number;
		/** Changes whenever the index behind `keyAt` changes. */
		version: number;
		activeIndex: number;
		label: string;
		keyAt: (index: number) => number | undefined;
		row: Snippet<[number, boolean]>;
		onrange: (range: Range, direction: number) => void;
		onselect: (index: number) => void;
		onactivate: (index: number) => void;
		initialOffset?: number;
		/**
		 * The top row (virtual top / rowHeight, fractional), reported on scroll and at
		 * teardown: a `{#key}` remount can mount the new list before the old one is torn down.
		 */
		onoffset?: (rows: number) => void;
		overscan?: number;
		maxSpacer?: number;
		autofocus?: boolean;
		/** Section headers between the rows; geometry is then in display slots. */
		layout?: SectionLayout;
		header?: Snippet<[Section]>;
		pullToRefresh?: PullToRefresh;
	}

	let {
		count,
		rowHeight,
		version,
		activeIndex,
		label,
		keyAt,
		row,
		onrange,
		onselect,
		onactivate,
		initialOffset = 0,
		onoffset,
		overscan = 6,
		maxSpacer,
		autofocus = false,
		layout,
		header,
		pullToRefresh
	}: Props = $props();

	const uid = $props.id();
	const headerId = (section: Section) => `${uid}-section-${section}`;
	const toDisplay = (index: number) => (layout ? layout.toDisplay(index) : index);

	let scroller: HTMLDivElement;
	let scrollTop = $state(0);
	let viewport = $state(0);
	let mounted = false;
	/** Plain, so teardown reports the row under the old height even when `rowHeight` changed. */
	let topRow = 0;

	const geometry: ListGeometry = $derived({
		count: layout ? layout.total : count,
		rowHeight,
		viewport,
		maxSpacer
	});
	const virtual = $derived(virtualTop(scrollTop, geometry));
	const range = $derived(visibleRange(virtual, geometry, overscan));
	type SlotItem =
		| { display: number; key: string; section: Section; index?: undefined }
		| { display: number; key: number; index: number };
	const rows = $derived.by(() => {
		void version;
		const out: SlotItem[] = [];
		for (let d = range.first; d <= range.last; d++) {
			const slot = layout ? layout.slotAt(d) : { kind: 'row' as const, index: d };
			if (slot.kind === 'header') {
				out.push({ display: d, key: `header:${slot.section}`, section: slot.section });
			} else {
				out.push({ display: d, index: slot.index, key: keyAt(slot.index) ?? -1 - slot.index });
			}
		}
		return out;
	});
	const activeKey = $derived.by(() => {
		void version;
		return activeIndex >= 0 ? keyAt(activeIndex) : undefined;
	});

	let lastVirtual = 0;
	$effect(() => {
		const r = range;
		const direction = Math.sign(virtual - lastVirtual);
		lastVirtual = virtual;
		void version;
		onrange(layout ? layout.toThreadRange(r) : r, direction);
	});

	let lastActive = -1;
	$effect(() => {
		const index = activeIndex;
		const g = geometry;
		if (!mounted || index === lastActive || g.viewport === 0) return;
		lastActive = index;
		if (index < 0) return;
		const target = revealTarget(toDisplay(index), virtualTop(scroller.scrollTop, g), g);
		if (target !== null) scroller.scrollTop = physicalTop(target, g);
	});

	onMount(() => {
		viewport = scroller.clientHeight;
		scroller.scrollTop = physicalTop(initialOffset, geometry);
		onscroll();
		lastActive = activeIndex;
		mounted = true;
		if (autofocus) scroller.focus({ preventScroll: true });
		return () => onoffset?.(topRow);
	});

	function onscroll() {
		scrollTop = scroller.scrollTop;
		topRow = virtualTop(scrollTop, geometry) / rowHeight;
		onoffset?.(topRow);
	}

	/** Where the indicator rests while a refresh runs. */
	const PULL_REST_PX = 48;
	let pullOffset = $state(0);
	let pulling = $state(false);
	const pull = new PullTracker(
		() => pullToRefresh?.enabled ?? false,
		() => scroller.scrollTop
	);
	const indicatorOffset = $derived(pulling ? pullOffset : pullToRefresh?.busy ? PULL_REST_PX : 0);

	function syncPull() {
		pullOffset = pull.offset;
		pulling = pull.pulling;
	}

	/** Pulls are possible; the non-passive listeners exist only then. */
	const pullEnabled = $derived(pullToRefresh?.enabled ?? false);

	// Non-passive listeners: Svelte's `ontouchmove` is passive, and a pull must
	// be able to `preventDefault()` the native pan. Without pulls (desktop,
	// selection mode) none are attached, so scrolling stays on passive listeners.
	$effect(() => {
		if (!pullEnabled) return;
		const el = scroller;
		const start = (e: TouchEvent) => {
			pull.start(e);
			syncPull();
		};
		const move = (e: TouchEvent) => {
			if (pull.move(e)) e.preventDefault();
			syncPull();
		};
		const end = (e: TouchEvent) => {
			const refresh = pull.end(e.touches.length);
			syncPull();
			if (refresh) pullToRefresh?.onrefresh();
		};
		const cancel = () => {
			pull.cancel();
			syncPull();
		};
		el.addEventListener('touchstart', start, { passive: false });
		el.addEventListener('touchmove', move, { passive: false });
		el.addEventListener('touchend', end);
		el.addEventListener('touchcancel', cancel);
		return () => {
			el.removeEventListener('touchstart', start);
			el.removeEventListener('touchmove', move);
			el.removeEventListener('touchend', end);
			el.removeEventListener('touchcancel', cancel);
			cancel();
		};
	});

	function onkeydown(event: KeyboardEvent) {
		if (event.ctrlKey || event.metaKey || event.altKey) return;
		const page = pageRows(geometry);
		const from = activeIndex < 0 ? -1 : activeIndex;
		const next: Record<string, number> = {
			ArrowDown: from + 1,
			ArrowUp: Math.max(0, from - 1),
			PageDown: from + page,
			PageUp: from - page,
			Home: 0,
			End: count - 1
		};
		if (!(event.key in next)) return;
		event.preventDefault();
		const to = clampIndex(next[event.key], count);
		if (to >= 0) onselect(to);
	}

	function onclick(event: MouseEvent) {
		const target = (event.target as Element).closest<HTMLElement>('[data-index]');
		if (!target) return;
		const index = Number(target.dataset.index);
		onselect(index);
		onactivate(index);
	}
</script>

<div class="frame">
	{#if pullToRefresh && indicatorOffset > 0}
		<div
			class="pull"
			class:dragging={pulling}
			class:busy={pullToRefresh.busy && !pulling}
			role="progressbar"
			aria-label="Refreshing"
			aria-valuemin={pulling ? 0 : undefined}
			aria-valuemax={pulling ? 100 : undefined}
			aria-valuenow={pulling
				? Math.round(Math.min(1, pullOffset / PULL_THRESHOLD_PX) * 100)
				: undefined}
			style:transform="translateY({indicatorOffset}px)"
		>
			<span class="spinner" style:--turn={Math.min(1, pullOffset / PULL_THRESHOLD_PX)}></span>
		</div>
	{/if}
	<div
		class="scroller"
		bind:this={scroller}
		bind:clientHeight={viewport}
		{onscroll}
		{onkeydown}
		{onclick}
		role="listbox"
		tabindex="0"
		aria-label={label}
		aria-activedescendant={activeKey !== undefined ? `thread-${activeKey}` : undefined}
	>
		{#if layout}
			<!-- Header slots scroll out of the DOM; options are described by these instead. -->
			{#each layout.sections as section (section)}
				<div hidden id={headerId(section)}>{@render header?.(section)}</div>
			{/each}
		{/if}
		<div class="spacer" style:height="{spacerHeight(geometry)}px">
			{#each rows as r (r.key)}
				{#if r.index === undefined}
					<div
						class="slot header"
						aria-hidden="true"
						style:height="{rowHeight}px"
						style:transform="translateY({rowOffset(r.display, scrollTop, geometry)}px)"
					>
						{@render header?.(r.section)}
					</div>
				{:else}
					<div
						class="slot"
						role="option"
						id={typeof r.key === 'number' && r.key >= 0 ? `thread-${r.key}` : undefined}
						data-index={r.index}
						aria-selected={r.index === activeIndex}
						aria-setsize={count}
						aria-posinset={r.index + 1}
						aria-describedby={layout ? headerId(layout.sectionOf(r.index)) : undefined}
						style:height="{rowHeight}px"
						style:transform="translateY({rowOffset(r.display, scrollTop, geometry)}px)"
					>
						{@render row(r.index, r.index === activeIndex)}
					</div>
				{/if}
			{/each}
		</div>
	</div>
</div>

<style>
	/* Holds the pull indicator beside the listbox and clips it above the top. */
	.frame {
		position: relative;
		height: 100%;
		overflow: hidden;
	}

	.scroller {
		position: relative;
		height: 100%;
		overflow-y: auto;
		overflow-x: hidden;
		contain: strict;
		overscroll-behavior: contain;
		outline: none;
	}

	.scroller:focus-visible {
		box-shadow: inset 0 0 0 2px var(--accent);
	}

	.spacer {
		position: relative;
		width: 100%;
	}

	/* Overlay outside the listbox: `.spacer` and the rows do not move during a pull. */
	.pull {
		position: absolute;
		top: -40px;
		left: 50%;
		z-index: 2;
		width: 36px;
		height: 36px;
		margin-left: -18px;
		display: grid;
		place-items: center;
		border-radius: 50%;
		background: var(--surface);
		box-shadow: 0 1px 4px rgb(0 0 0 / 0.25);
		pointer-events: none;
		transition: transform 0.2s ease-out;
	}

	.pull.dragging {
		transition: none;
	}

	.spinner {
		width: 18px;
		height: 18px;
		border: 2px solid var(--accent);
		border-right-color: transparent;
		border-radius: 50%;
		transform: rotate(calc(var(--turn) * 270deg));
	}

	.pull.busy .spinner {
		animation: pull-spin 0.8s linear infinite;
	}

	@keyframes pull-spin {
		to {
			transform: rotate(360deg);
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.pull,
		.pull.busy .spinner {
			transition: none;
			animation-duration: 2.4s;
		}
	}

	.slot {
		position: absolute;
		top: 0;
		left: 0;
		right: 0;
		will-change: transform;
		cursor: pointer;
	}

	.slot.header {
		cursor: default;
	}
</style>
