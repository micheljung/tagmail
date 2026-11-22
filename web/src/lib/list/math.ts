/**
 * Spacer heights above this are scaled: Firefox caps element heights near
 * 17.9M px and WebKit at a similar order, Chromium near 33M.
 */
export const MAX_SPACER_PX = 15_000_000;

export interface ListGeometry {
	count: number;
	rowHeight: number;
	/** Visible height of the scroller in px. */
	viewport: number;
	maxSpacer?: number;
}

export function totalHeight(g: ListGeometry): number {
	return g.count * g.rowHeight;
}

export function spacerHeight(g: ListGeometry): number {
	return Math.min(totalHeight(g), g.maxSpacer ?? MAX_SPACER_PX);
}

/** Ratio of virtual px per physical scroll px (1 unless the spacer is capped). */
export function scrollScale(g: ListGeometry): number {
	const total = totalHeight(g);
	const spacer = spacerHeight(g);
	if (total <= spacer) return 1;
	const physical = spacer - g.viewport;
	return physical > 0 ? (total - g.viewport) / physical : 1;
}

/** Physical `scrollTop` → offset into the full virtual list. */
export function virtualTop(scrollTop: number, g: ListGeometry): number {
	return scrollTop * scrollScale(g);
}

/** Offset into the virtual list → physical `scrollTop`. */
export function physicalTop(virtual: number, g: ListGeometry): number {
	const max = Math.max(0, totalHeight(g) - g.viewport);
	return Math.min(max, Math.max(0, virtual)) / scrollScale(g);
}

export interface Range {
	first: number;
	/** Inclusive; `last < first` when the list is empty. */
	last: number;
}

export function visibleRange(virtual: number, g: ListGeometry, overscan: number): Range {
	if (g.count === 0) return { first: 0, last: -1 };
	const first = Math.max(0, Math.floor(virtual / g.rowHeight) - overscan);
	const last = Math.min(g.count - 1, Math.ceil((virtual + g.viewport) / g.rowHeight) + overscan);
	return { first, last: Math.max(first, last) };
}

/** Row `index`'s top inside the spacer, given the current scroll position. */
export function rowOffset(index: number, scrollTop: number, g: ListGeometry): number {
	return index * g.rowHeight - virtualTop(scrollTop, g) + scrollTop;
}

/**
 * The virtual offset that brings `index` fully into view with the least
 * movement, or `null` if it already is.
 */
export function revealTarget(index: number, virtual: number, g: ListGeometry): number | null {
	const top = index * g.rowHeight;
	const bottom = top + g.rowHeight;
	if (top < virtual) return top;
	if (bottom > virtual + g.viewport) return bottom - g.viewport;
	return null;
}

/** Rows per keyboard page, never less than one. */
export function pageRows(g: ListGeometry): number {
	return Math.max(1, Math.floor(g.viewport / g.rowHeight) - 1);
}

export function clampIndex(index: number, count: number): number {
	return count === 0 ? -1 : Math.min(count - 1, Math.max(0, index));
}
