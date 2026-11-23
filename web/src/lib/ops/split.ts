import type { ThreadRowDto } from '@tagmail/api-types';

/**
 * Thread ids grouped by account, in their order: the server takes ops for
 * one account at a time. Threads without a known row share one group.
 */
export function byAccount(
	threadIds: readonly number[],
	rows: readonly Pick<ThreadRowDto, 'id' | 'accountId'>[]
): number[][] {
	const accountOf = new Map(rows.map((r) => [r.id, r.accountId]));
	const groups = new Map<number | undefined, number[]>();
	for (const id of threadIds) {
		const account = accountOf.get(id);
		groups.set(account, [...(groups.get(account) ?? []), id]);
	}
	return [...groups.values()];
}
