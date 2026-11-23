import type { ThreadRowDto } from '@tagmail/api-types';
import { describe, expect, it } from 'vitest';
import { describeDone, inverseOf, joinsView, leavesView, patchRow, type OpIntent } from './effects';
import { findByName, labelChanges, labelState, nextState, type TriState } from './labels';

const ctx = { inboxIds: new Set([1]) };

const ROW: ThreadRowDto = {
	id: 7,
	accountId: 1,
	subject: 's',
	snippet: '',
	lastDate: 1,
	messageCount: 2,
	unreadCount: 2,
	starred: false,
	hasAttachment: false,
	senders: [],
	labels: ['b'],
	mailboxIds: [1]
};

const intent = (kind: OpIntent['kind'], extra: Partial<OpIntent> = {}): OpIntent => ({
	kind,
	threadIds: [7],
	...extra
});

describe('patchRow', () => {
	it('applies idempotent setters', () => {
		expect(patchRow(ROW, intent('label.add', { label: 'a' })).labels).toEqual(['a', 'b']);
		expect(patchRow(ROW, intent('label.add', { label: 'b' }))).toBe(ROW);
		expect(patchRow(ROW, intent('label.remove', { label: 'b' })).labels).toEqual([]);
		expect(patchRow(ROW, intent('markRead')).unreadCount).toBe(0);
		expect(patchRow({ ...ROW, unreadCount: 0 }, intent('markUnread')).unreadCount).toBe(1);
		expect(patchRow(ROW, intent('markUnread'))).toBe(ROW);
		expect(patchRow(ROW, intent('star')).starred).toBe(true);
		expect(patchRow(ROW, intent('archive'))).toBe(ROW);
	});
});

describe('list membership', () => {
	it('archive leaves the inbox but not labels or searches', () => {
		expect(leavesView(intent('archive'), 'inbox', ctx)).toBe(true);
		expect(leavesView(intent('archive'), 'mailbox:1', ctx)).toBe(true);
		expect(leavesView(intent('archive'), 'label:b', ctx)).toBe(false);
		expect(leavesView(intent('archive'), 'search:foo', ctx)).toBe(false);
	});

	it('delete and spam leave every list but their target', () => {
		for (const view of ['inbox', 'label:b', 'search:x', 'mailbox:3'] as const) {
			expect(leavesView(intent('trash'), view, ctx)).toBe(true);
		}
		expect(leavesView(intent('trash', { mailboxId: 4 }), 'mailbox:4', ctx)).toBe(false);
	});

	it('moves leave the source folder and join the target', () => {
		const move = intent('move', { mailboxId: 6 });
		expect(leavesView(move, 'inbox', ctx)).toBe(true);
		expect(leavesView(move, 'mailbox:3', ctx)).toBe(true);
		expect(leavesView(move, 'mailbox:6', ctx)).toBe(false);
		expect(joinsView(move, 'mailbox:6', ctx)).toBe(true);
		expect(joinsView(intent('unarchive'), 'inbox', ctx)).toBe(true);
		expect(joinsView(intent('label.add', { label: 'z' }), 'label:z', ctx)).toBe(true);
	});

	it('only removing the filtered label leaves a label list', () => {
		expect(leavesView(intent('label.remove', { label: 'b' }), 'label:b', ctx)).toBe(true);
		expect(leavesView(intent('label.remove', { label: 'c' }), 'label:b', ctx)).toBe(false);
	});
});

describe('inverseOf', () => {
	it('pairs reversible ops', () => {
		expect(inverseOf(intent('star'), 'inbox', ctx)?.[0].kind).toBe('unstar');
		expect(inverseOf(intent('label.add', { label: 'x' }), 'inbox', ctx)).toEqual([
			intent('label.remove', { label: 'x' })
		]);
		expect(inverseOf(intent('archive'), 'label:b', ctx)?.[0].kind).toBe('unarchive');
	});

	it('without origins undoes moves only where the list names the folder', () => {
		expect(inverseOf(intent('trash'), 'inbox', ctx)?.[0].kind).toBe('unarchive');
		expect(inverseOf(intent('trash'), 'mailbox:3', ctx)).toEqual([
			intent('move', { mailboxId: 3 })
		]);
		expect(inverseOf(intent('trash'), 'label:b', ctx)).toBeNull();
		expect(inverseOf(intent('delete'), 'mailbox:4', ctx)).toBeNull();
	});

	it('with origins returns every thread to where it was', () => {
		const origins = new Map([
			[1, [1, 3]],
			[2, [3]],
			[3, [6]],
			[4, [3]]
		]);
		const trash: OpIntent = { kind: 'trash', threadIds: [1, 2, 3, 4] };
		expect(inverseOf(trash, 'search:x', ctx, origins)).toEqual([
			{ kind: 'unarchive', threadIds: [1] },
			{ kind: 'move', mailboxId: 3, threadIds: [2, 4] },
			{ kind: 'move', mailboxId: 6, threadIds: [3] }
		]);
		const move: OpIntent = { kind: 'move', mailboxId: 6, threadIds: [3] };
		expect(inverseOf(move, 'label:b', ctx, new Map([[3, [6, 3]]]))).toEqual([
			{ kind: 'move', mailboxId: 3, threadIds: [3] }
		]);
		expect(inverseOf(trash, 'label:b', ctx, new Map([[1, [1]]]))).toBeNull();
	});
});

it('describes ops in player words', () => {
	const names = { label: (k: string) => k.toUpperCase(), mailbox: () => 'Projects' };
	expect(describeDone({ kind: 'move', threadIds: [1, 2], mailboxId: 6 }, names)).toBe(
		'Moved 2 conversations to Projects'
	);
	expect(describeDone(intent('label.add', { label: 'x' }), names)).toBe(
		'Added “X” to 1 conversation'
	);
});

describe('label tri-state', () => {
	const threads = [['a', 'b'], ['a'], []];

	it('reads all, some or none across the targets', () => {
		expect(labelState('a', threads.slice(0, 2))).toBe('all');
		expect(labelState('a', threads)).toBe('some');
		expect(labelState('z', threads)).toBe('none');
	});

	it('cycles a mixed label through all, none and back to mixed', () => {
		let s: TriState = 'some';
		const seen: TriState[] = [];
		for (let i = 0; i < 3; i++) seen.push((s = nextState('some', s)));
		expect(seen).toEqual(['all', 'none', 'some']);
		expect(nextState('none', 'none')).toBe('all');
		expect(nextState('all', 'none')).toBe('all');
		expect(nextState('none', 'all')).toBe('none');
	});

	it('turns a picker session into add and remove ops, leaving mixed labels alone', () => {
		const initial = new Map<string, TriState>([
			['a', 'some'],
			['b', 'all'],
			['c', 'none'],
			['d', 'some']
		]);
		const final = new Map<string, TriState>([
			['a', 'all'],
			['b', 'none'],
			['c', 'none'],
			['d', 'some']
		]);
		expect(labelChanges(initial, final)).toEqual({ add: ['a'], remove: ['b'] });
		expect(labelChanges(initial, new Map([['new', 'all']]))).toEqual({ add: ['new'], remove: [] });
	});

	it('matches existing names case-insensitively', () => {
		const labels = [{ name: 'Finance' }];
		expect(findByName(labels, ' finance ')).toBe(labels[0]);
		expect(findByName(labels, 'fin')).toBeUndefined();
	});
});
