import { decodeIdIndex } from '@tagmail/api-types';
import { describe, expect, it } from 'vitest';
import { encodeIdIndex } from '../../../e2e/mock/idIndex';
import { ThreadIndex, insertionPoint } from './threadIndex';

describe('id index wire format', () => {
	it('round-trips non-monotonic ids including large gaps', () => {
		const ids = [5, 3, 1_000_000, 2 ** 40, 7, 0, Number.MAX_SAFE_INTEGER, 64, 63, 8192];
		expect(decodeIdIndex(encodeIdIndex(ids))).toEqual(ids);
	});

	it('encodes 100k jittered ids in about 2 bytes each', () => {
		const ids = Array.from({ length: 100_000 }, (_, i) => 200_000 - i * 2 + ((i * 7919) % 40));
		const bytes = encodeIdIndex(ids);
		expect(bytes.length).toBeLessThan(250_000);
		expect(decodeIdIndex(bytes)).toEqual(ids);
	});

	it('rejects a truncated varint', () => {
		expect(() => decodeIdIndex(Uint8Array.from([0x80]))).toThrow(/ends inside a varint/);
	});
});

describe('ThreadIndex', () => {
	it('finds, removes and inserts ids', () => {
		const index = new ThreadIndex(Float64Array.from([10, 20, 30]));
		expect(index.indexOf(20)).toBe(1);
		expect(index.remove(20)).toBe(true);
		expect(index.remove(99)).toBe(false);
		expect(index.slice(0, 5)).toEqual([10, 30]);
		index.insert(0, 40);
		expect(index.slice(0, 5)).toEqual([40, 10, 30]);
		expect(index.indexOf(30)).toBe(2);
		expect(index.at(5)).toBeUndefined();
	});
});

describe('insertionPoint', () => {
	const dates = [100, 90, 90, 80, 70, 60];
	const known = (i: number) => dates[i];

	it('puts newer threads at the top', () => {
		expect(insertionPoint(6, 150, known, [{ first: 0, last: 5 }])).toBe(0);
	});

	it('puts a thread before rows of the same date', () => {
		expect(insertionPoint(6, 90, known, [{ first: 0, last: 5 }])).toBe(1);
		expect(insertionPoint(6, 85, known, [{ first: 0, last: 5 }])).toBe(3);
	});

	it('appends past the oldest row', () => {
		expect(insertionPoint(6, 10, known, [{ first: 0, last: 5 }])).toBe(6);
	});

	it('handles an empty list', () => {
		expect(insertionPoint(0, 10, known, [])).toBe(0);
	});

	it('reports a gap when the spot lies outside loaded rows', () => {
		const partial = (i: number) => (i >= 2 && i <= 3 ? dates[i] : undefined);
		expect(insertionPoint(6, 95, partial, [{ first: 2, last: 3 }])).toBeNull();
		expect(insertionPoint(6, 65, partial, [{ first: 2, last: 3 }])).toBeNull();
		expect(insertionPoint(6, 85, partial, [{ first: 2, last: 3 }])).toBe(3);
	});

	it('falls through to the next known range', () => {
		const partial = (i: number) => (i <= 1 || i >= 4 ? dates[i] : undefined);
		expect(
			insertionPoint(6, 65, partial, [
				{ first: 0, last: 1 },
				{ first: 4, last: 5 }
			])
		).toBe(5);
	});
});
