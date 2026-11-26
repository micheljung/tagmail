import { describe, expect, it } from 'vitest';
import {
	clampNavWidth,
	parseDensity,
	parseInboxType,
	parseReadingPane,
	rowHeightFor,
	type Density
} from './prefs';

describe('prefs parsing', () => {
	it('accepts known values and falls back to the default otherwise', () => {
		expect(parseDensity('compact')).toBe('compact');
		expect(parseDensity('comfortable')).toBe('comfortable');
		expect(parseDensity('huge')).toBe('default');
		expect(parseDensity(null)).toBe('default');
		expect(parseInboxType('unread')).toBe('unread');
		expect(parseInboxType('x')).toBe('default');
		expect(parseReadingPane('right')).toBe('right');
		expect(parseReadingPane('below')).toBe('below');
		expect(parseReadingPane(undefined)).toBe('none');
	});

	it('clamps the nav width to 200-400', () => {
		expect(clampNavWidth(100)).toBe(200);
		expect(clampNavWidth(600)).toBe(400);
		expect(clampNavWidth('300')).toBe(300);
		expect(clampNavWidth(250.6)).toBe(251);
	});

	it('uses 236 for missing or invalid widths', () => {
		for (const v of [null, undefined, '', 'abc', NaN, Infinity]) expect(clampNavWidth(v)).toBe(236);
	});
});

describe('rowHeightFor', () => {
	const desktop = { coarse: false, phone: false };
	const coarse = { coarse: true, phone: false };
	const phone = { coarse: false, phone: true };

	it("keeps today's heights for the default density", () => {
		expect(rowHeightFor('default', desktop)).toBe(40);
		expect(rowHeightFor('default', coarse)).toBe(48);
		expect(rowHeightFor('default', phone)).toBe(76);
	});

	it('follows the density table', () => {
		expect([desktop, coarse, phone].map((c) => rowHeightFor('comfortable', c))).toEqual([
			48, 56, 88
		]);
		expect([desktop, coarse, phone].map((c) => rowHeightFor('compact', c))).toEqual([32, 44, 64]);
	});

	it('gives three distinct heights in every context', () => {
		for (const ctx of [desktop, coarse, phone]) {
			const heights = (['compact', 'default', 'comfortable'] as Density[]).map((d) =>
				rowHeightFor(d, ctx)
			);
			expect(new Set(heights).size).toBe(3);
		}
	});

	it('lets a phone win over a coarse pointer', () => {
		expect(rowHeightFor('default', { coarse: true, phone: true })).toBe(76);
	});
});
