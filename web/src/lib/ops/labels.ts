/** A label's state across the threads an action targets. */
export type TriState = 'all' | 'some' | 'none';

export function labelState(key: string, threadLabels: readonly (readonly string[])[]): TriState {
	let have = 0;
	for (const labels of threadLabels) if (labels.includes(key)) have++;
	if (have === 0) return 'none';
	return have === threadLabels.length ? 'all' : 'some';
}

/**
 * Click cycle, Gmail style: a label that started mixed goes all → none →
 * back to mixed (leave as is); others just flip.
 */
export function nextState(initial: TriState, current: TriState): TriState {
	if (current === 'all') return 'none';
	if (current === 'none') return initial === 'some' ? 'some' : 'all';
	return 'all';
}

export interface LabelChanges {
	add: string[];
	remove: string[];
}

/** The ops a picker session asks for: only labels whose state moved to all or none. */
export function labelChanges(
	initial: ReadonlyMap<string, TriState>,
	final: ReadonlyMap<string, TriState>
): LabelChanges {
	const add: string[] = [];
	const remove: string[] = [];
	for (const [key, state] of final) {
		const before = initial.get(key) ?? 'none';
		if (state === before || state === 'some') continue;
		(state === 'all' ? add : remove).push(key);
	}
	return { add, remove };
}

/** Exact (case-insensitive) name match, so typing an existing name never creates a twin. */
export function findByName<T extends { name: string }>(
	labels: readonly T[],
	name: string
): T | undefined {
	const want = name.trim().toLocaleLowerCase();
	return labels.find((l) => l.name.toLocaleLowerCase() === want);
}
