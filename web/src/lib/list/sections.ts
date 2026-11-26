import type { Range } from './math';
import { ThreadIndex } from './threadIndex';

export type Section = 'unread' | 'rest';

/** What a display slot shows: a section header or the thread at `index`. */
export type Slot = { kind: 'header'; section: Section } | { kind: 'row'; index: number };

/**
 * `base` (newest first) split into the unread threads, then the rest, each
 * keeping base order. Ids in `held` stay in the unread section.
 */
export function partition(
	base: ThreadIndex,
	member: (id: number) => boolean,
	held?: ReadonlySet<number>
): { index: ThreadIndex; split: number } {
	const n = base.length;
	const ids = new Float64Array(n);
	const rest = new Float64Array(n);
	let split = 0;
	let tail = 0;
	for (let i = 0; i < n; i++) {
		const id = base.at(i)!;
		if (member(id) || held?.has(id)) ids[split++] = id;
		else rest[tail++] = id;
	}
	ids.set(rest.subarray(0, tail), split);
	return { index: new ThreadIndex(ids), split };
}

/**
 * Display geometry of a sectioned list: one header slot before each
 * non-empty section. Thread indexes are positions in the partitioned index.
 */
export interface SectionLayout {
	/** Display slots, headers included. */
	readonly total: number;
	/** The non-empty sections, in display order. */
	readonly sections: readonly Section[];
	slotAt(display: number): Slot;
	toDisplay(index: number): number;
	/** The threads inside a display range. */
	toThreadRange(range: Range): Range;
	sectionOf(index: number): Section;
}

export function sectionLayout(count: number, split: number): SectionLayout {
	const unreadHeader = split > 0 ? 1 : 0;
	const restHeader = count > split ? 1 : 0;
	/** Display slot of the rest header. */
	const restAt = unreadHeader + split;
	const total = count + unreadHeader + restHeader;
	const toDisplay = (index: number) =>
		index < split ? index + unreadHeader : index + unreadHeader + restHeader;
	/** Thread index of the first row at or after `display`. */
	const ceilThread = (display: number) => {
		if (display < restAt) return Math.max(0, display - unreadHeader);
		return display < restAt + restHeader ? split : display - unreadHeader - restHeader;
	};
	const sections: Section[] = [];
	if (unreadHeader) sections.push('unread');
	if (restHeader) sections.push('rest');
	return {
		total,
		sections,
		slotAt(display) {
			if (unreadHeader && display === 0) return { kind: 'header', section: 'unread' };
			if (restHeader && display === restAt) return { kind: 'header', section: 'rest' };
			return {
				kind: 'row',
				index: display < restAt ? display - unreadHeader : display - unreadHeader - restHeader
			};
		},
		toDisplay,
		toThreadRange({ first, last }) {
			if (last < first) return { first: 0, last: -1 };
			const from = ceilThread(Math.max(0, first));
			// The last row at or before `last`: a header slot maps to the row above it.
			const to = Math.min(
				count - 1,
				last < restAt ? last - unreadHeader : last - unreadHeader - restHeader
			);
			return to < from ? { first: from, last: from - 1 } : { first: from, last: to };
		},
		sectionOf: (index) => (index < split ? 'unread' : 'rest')
	};
}
