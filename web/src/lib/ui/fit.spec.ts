import { describe, expect, it } from 'vitest';
import { fitScale } from './fit';

describe('fitScale', () => {
	it('keeps content that fits at scale 1', () => {
		expect(fitScale(300, 390)).toBe(1);
		expect(fitScale(390, 390)).toBe(1);
	});

	it('ignores overflow of up to 1px', () => {
		expect(fitScale(391, 390)).toBe(1);
		expect(fitScale(391.5, 390)).not.toBe(1);
	});

	it('scales wider content to the available width, rounded down to 3 decimals', () => {
		expect(fitScale(600, 390)).toBe(0.65);
		expect(fitScale(600, 412)).toBe(0.686);
		expect(fitScale(1200, 400)).toBe(0.333);
	});

	it('never rounds the scaled width past the available width', () => {
		for (const natural of [601, 777, 1234, 2999]) {
			expect(natural * fitScale(natural, 390)).toBeLessThanOrEqual(390);
		}
	});

	it('does not scale before the container has a width', () => {
		expect(fitScale(600, 0)).toBe(1);
	});
});
