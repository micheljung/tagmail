import { describe, expect, it } from 'vitest';
import {
	clampIndex,
	pageRows,
	physicalTop,
	revealTarget,
	rowOffset,
	scrollScale,
	spacerHeight,
	virtualTop,
	visibleRange,
	type ListGeometry
} from './math';

const g: ListGeometry = { count: 100_000, rowHeight: 40, viewport: 800 };

describe('list math', () => {
	it('keeps 100k rows unscaled', () => {
		expect(spacerHeight(g)).toBe(4_000_000);
		expect(scrollScale(g)).toBe(1);
		expect(virtualTop(1234, g)).toBe(1234);
		expect(rowOffset(99_000, 99_000 * 40, g)).toBe(99_000 * 40);
	});

	it('computes the rendered range with overscan, clamped to the list', () => {
		expect(visibleRange(99_000 * 40, g, 6)).toEqual({ first: 98_994, last: 99_026 });
		expect(visibleRange(0, g, 6)).toEqual({ first: 0, last: 26 });
		expect(visibleRange(3_999_200, g, 6)).toEqual({ first: 99_974, last: 99_999 });
		expect(visibleRange(0, { ...g, count: 0 }, 6)).toEqual({ first: 0, last: -1 });
	});

	it('scales scrolling once the spacer hits the cap', () => {
		const big: ListGeometry = {
			count: 1_000_000,
			rowHeight: 40,
			viewport: 800,
			maxSpacer: 1_000_000
		};
		expect(spacerHeight(big)).toBe(1_000_000);
		const maxScroll = 1_000_000 - 800;
		expect(virtualTop(maxScroll, big)).toBeCloseTo(40_000_000 - 800, 3);
		expect(physicalTop(40_000_000 - 800, big)).toBeCloseTo(maxScroll, 3);
		const scrollTop = 500_000;
		const top = rowOffset(Math.floor(virtualTop(scrollTop, big) / 40), scrollTop, big);
		expect(top).toBeGreaterThan(scrollTop - 40);
		expect(top).toBeLessThanOrEqual(scrollTop);
	});

	it('reveals rows above and below the viewport', () => {
		expect(revealTarget(10, 800, g)).toBe(400);
		expect(revealTarget(50, 0, g)).toBe(50 * 40 + 40 - 800);
		expect(revealTarget(5, 0, g)).toBeNull();
	});

	it('pages and clamps', () => {
		expect(pageRows(g)).toBe(19);
		expect(pageRows({ ...g, viewport: 10 })).toBe(1);
		expect(clampIndex(-3, 10)).toBe(0);
		expect(clampIndex(30, 10)).toBe(9);
		expect(clampIndex(0, 0)).toBe(-1);
	});
});
