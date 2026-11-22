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

	interface Props {
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
		onoffset?: (virtual: number) => void;
		overscan?: number;
		maxSpacer?: number;
		autofocus?: boolean;
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
		autofocus = false
	}: Props = $props();

	let scroller: HTMLDivElement;
	let scrollTop = $state(0);
	let viewport = $state(0);
	let mounted = false;

	const geometry: ListGeometry = $derived({ count, rowHeight, viewport, maxSpacer });
	const virtual = $derived(virtualTop(scrollTop, geometry));
	const range = $derived(visibleRange(virtual, geometry, overscan));
	const rows = $derived.by(() => {
		void version;
		const out: { index: number; key: number }[] = [];
		for (let i = range.first; i <= range.last; i++) out.push({ index: i, key: keyAt(i) ?? -1 - i });
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
		onrange(r, direction);
	});

	let lastActive = -1;
	$effect(() => {
		const index = activeIndex;
		const g = geometry;
		if (!mounted || index === lastActive || g.viewport === 0) return;
		lastActive = index;
		if (index < 0) return;
		const target = revealTarget(index, virtualTop(scroller.scrollTop, g), g);
		if (target !== null) scroller.scrollTop = physicalTop(target, g);
	});

	onMount(() => {
		viewport = scroller.clientHeight;
		scroller.scrollTop = physicalTop(initialOffset, { count, rowHeight, viewport, maxSpacer });
		scrollTop = scroller.scrollTop;
		lastActive = activeIndex;
		mounted = true;
		if (autofocus) scroller.focus({ preventScroll: true });
		return () => onoffset?.(virtualTop(scrollTop, geometry));
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

<div
	class="scroller"
	bind:this={scroller}
	bind:clientHeight={viewport}
	onscroll={() => (scrollTop = scroller.scrollTop)}
	{onkeydown}
	{onclick}
	role="listbox"
	tabindex="0"
	aria-label={label}
	aria-activedescendant={activeKey !== undefined ? `thread-${activeKey}` : undefined}
>
	<div class="spacer" style:height="{spacerHeight(geometry)}px">
		{#each rows as r (r.key)}
			<div
				class="slot"
				role="option"
				id={r.key >= 0 ? `thread-${r.key}` : undefined}
				data-index={r.index}
				aria-selected={r.index === activeIndex}
				aria-setsize={count}
				aria-posinset={r.index + 1}
				style:height="{rowHeight}px"
				style:transform="translateY({rowOffset(r.index, scrollTop, geometry)}px)"
			>
				{@render row(r.index, r.index === activeIndex)}
			</div>
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
</style>
