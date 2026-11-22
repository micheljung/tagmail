import { describe, expect, it } from 'vitest';
import { parseCursor } from './cursor';

describe('parseCursor', () => {
	it('splits epoch and sequence', () => {
		expect(parseCursor('00000000000000ab:42')).toEqual({ epoch: '00000000000000ab', seq: 42 });
	});

	it('rejects malformed cursors', () => {
		for (const bad of ['', null, undefined, '42', ':4', 'a:', 'a:-1', 'a:1.5']) {
			expect(parseCursor(bad)).toBeNull();
		}
	});
});
