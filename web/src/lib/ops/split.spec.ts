import { describe, expect, it } from 'vitest';
import { byAccount } from './split';

describe('byAccount', () => {
	it('sends one op per account, keeping the selection order', () => {
		const rows = [
			{ id: 1, accountId: 1 },
			{ id: 2, accountId: 2 },
			{ id: 3, accountId: 1 },
			{ id: 4, accountId: 2 }
		];
		expect(byAccount([4, 1, 2, 3], rows)).toEqual([
			[4, 2],
			[1, 3]
		]);
	});

	it('keeps a single account in one op and groups threads without rows together', () => {
		expect(byAccount([5, 6], [{ id: 5, accountId: 1 }])).toEqual([[5], [6]]);
		expect(
			byAccount(
				[7, 8],
				[
					{ id: 7, accountId: 3 },
					{ id: 8, accountId: 3 }
				]
			)
		).toEqual([[7, 8]]);
	});
});
