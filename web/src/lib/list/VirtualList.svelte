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
	import type { Section, SectionLayout } from './sections';

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
		header
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

<style>
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
