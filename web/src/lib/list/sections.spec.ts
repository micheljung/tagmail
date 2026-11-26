import { describe, expect, it } from 'vitest';
import { partition, sectionLayout } from './sections';
import { ThreadIndex } from './threadIndex';

const index = (ids: number[]) => new ThreadIndex(Float64Array.from(ids));

describe('partition', () => {
	it('puts unread threads first, each section in base order', () => {
		const unread = new Set([40, 10]);
		const { index: out, split } = partition(index([50, 40, 30, 20, 10]), (id) => unread.has(id));
		expect(out.slice(0, 5)).toEqual([40, 10, 50, 30, 20]);
		expect(split).toBe(2);
	});

	it('keeps held ids in the unread section', () => {
		const { index: out, split } = partition(index([50, 40, 30]), (id) => id === 30, new Set([50]));
		expect(out.slice(0, 3)).toEqual([50, 30, 40]);
		expect(split).toBe(2);
	});

	it('handles empty and one-sided lists', () => {
		expect(partition(index([]), () => true).split).toBe(0);
		expect(partition(index([1, 2]), () => true).split).toBe(2);
		expect(partition(index([1, 2]), () => false).index.slice(0, 2)).toEqual([1, 2]);
	});

	it('stays linear on 100k threads', () => {
		const ids = Float64Array.from({ length: 100_000 }, (_, i) => 100_000 - i);
		const base = new ThreadIndex(ids);
		const member = (id: number) => id % 3 === 0;
		partition(base, member);
		const start = performance.now();
		for (let i = 0; i < 10; i++) partition(base, member);
		const each = (performance.now() - start) / 10;
		expect(each).toBeLessThan(50);
	});
});

describe('sectionLayout', () => {
	it('adds a header before each non-empty section', () => {
		const l = sectionLayout(5, 2);
		expect(l.total).toBe(7);
		expect([0, 1, 2, 3, 4, 5, 6].map((d) => l.slotAt(d))).toEqual([
			{ kind: 'header', section: 'unread' },
			{ kind: 'row', index: 0 },
			{ kind: 'row', index: 1 },
			{ kind: 'header', section: 'rest' },
			{ kind: 'row', index: 2 },
			{ kind: 'row', index: 3 },
			{ kind: 'row', index: 4 }
		]);
		expect([0, 1, 2, 3, 4].map((i) => l.toDisplay(i))).toEqual([1, 2, 4, 5, 6]);
		expect([0, 1, 2].map((i) => l.sectionOf(i))).toEqual(['unread', 'unread', 'rest']);
		expect(l.sections).toEqual(['unread', 'rest']);
		expect(sectionLayout(3, 0).sections).toEqual(['rest']);
	});

	it('drops the header of an empty section', () => {
		const none = sectionLayout(3, 0);
		expect(none.total).toBe(4);
		expect(none.slotAt(0)).toEqual({ kind: 'header', section: 'rest' });
		expect(none.toDisplay(0)).toBe(1);
		const all = sectionLayout(3, 3);
		expect(all.total).toBe(4);
		expect(all.slotAt(3)).toEqual({ kind: 'row', index: 2 });
		expect(all.toDisplay(2)).toBe(3);
		expect(sectionLayout(0, 0).total).toBe(0);
	});

	it('converts display ranges to the threads inside them', () => {
		const l = sectionLayout(5, 2);
		expect(l.toThreadRange({ first: 0, last: 6 })).toEqual({ first: 0, last: 4 });
		expect(l.toThreadRange({ first: 0, last: 0 })).toEqual({ first: 0, last: -1 });
		expect(l.toThreadRange({ first: 2, last: 3 })).toEqual({ first: 1, last: 1 });
		expect(l.toThreadRange({ first: 3, last: 4 })).toEqual({ first: 2, last: 2 });
		expect(l.toThreadRange({ first: 0, last: -1 })).toEqual({ first: 0, last: -1 });
		const none = sectionLayout(3, 0);
		expect(none.toThreadRange({ first: 0, last: 1 })).toEqual({ first: 0, last: 0 });
	});

	it('round-trips every thread index', () => {
		for (const [count, split] of [
			[6, 0],
			[6, 3],
			[6, 6]
		]) {
			const l = sectionLayout(count, split);
			for (let i = 0; i < count; i++)
				expect(l.slotAt(l.toDisplay(i))).toEqual({ kind: 'row', index: i });
		}
	});
});
