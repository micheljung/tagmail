import type { ThreadRowDto } from '@tagmail/api-types';
import { describe, expect, it, vi } from 'vitest';
import { RowCache } from './rowCache.svelte';

function row(id: number): ThreadRowDto {
	return {
		id,
		accountId: 1,
		subject: `s${id}`,
		snippet: '',
		lastDate: 1000 - id,
		messageCount: 1,
		unreadCount: 0,
		starred: false,
		hasAttachment: false,
		senders: [],
		labels: []
	};
}

interface Call {
	ids: number[];
	signal: AbortSignal;
	resolve: (rows: ThreadRowDto[]) => void;
}

function harness(capacity?: number) {
	const calls: Call[] = [];
	const onMissing = vi.fn();
	const cache = new RowCache(
		(ids, signal) => new Promise((resolve) => calls.push({ ids, signal, resolve })),
		{ capacity, onMissing }
	);
	const settle = async (call: Call, skip: number[] = []) => {
		call.resolve(call.ids.filter((id) => !skip.includes(id)).map(row));
		await Promise.resolve();
		await Promise.resolve();
	};
	return { cache, calls, onMissing, settle };
}

const range = (a: number, b: number) => Array.from({ length: b - a }, (_, i) => a + i);

describe('RowCache', () => {
	it('fetches missing ids in pages of 100 and dedupes in-flight ids', async () => {
		const { cache, calls, settle } = harness();
		cache.ensure(range(0, 150));
		cache.ensure(range(0, 150));
		expect(calls.map((c) => c.ids.length)).toEqual([100, 50]);
		await settle(calls[0]);
		expect(cache.peek(5)?.subject).toBe('s5');
		expect(cache.version).toBe(1);
		cache.ensure(range(0, 150));
		expect(calls).toHaveLength(2);
	});

	it('aborts requests whose rows all left the window', () => {
		const { cache, calls } = harness();
		cache.ensure(range(0, 100));
		cache.ensure(range(5000, 5100));
		expect(calls[0].signal.aborted).toBe(true);
		expect(calls[1].signal.aborted).toBe(false);
		expect(cache.isPending(1)).toBe(false);
	});

	it('evicts least recently used rows past capacity', async () => {
		const { cache, calls, settle } = harness(150);
		cache.ensure(range(0, 100));
		await settle(calls[0]);
		cache.ensure(range(0, 10));
		cache.ensure(range(100, 200));
		await settle(calls[1]);
		expect(cache.size).toBe(150);
		expect(cache.peek(5)).toBeDefined();
		expect(cache.peek(50)).toBeUndefined();
		expect(cache.peek(150)).toBeDefined();
	});

	it('reports ids the server no longer has', async () => {
		const { cache, calls, onMissing, settle } = harness();
		cache.ensure([1, 2, 3]);
		await settle(calls[0], [2]);
		expect(onMissing).toHaveBeenCalledWith([2]);
	});

	it('keeps showing an invalidated row while it refetches', async () => {
		const { cache, calls, settle } = harness();
		cache.ensure([1]);
		await settle(calls[0]);
		cache.invalidate(1);
		expect(cache.peek(1)).toBeDefined();
		expect(cache.isStale(1)).toBe(true);
		cache.ensure([1]);
		expect(calls).toHaveLength(2);
		cache.ensure([1]);
		expect(calls).toHaveLength(2);
		await settle(calls[1]);
		expect(cache.isStale(1)).toBe(false);
	});
});
