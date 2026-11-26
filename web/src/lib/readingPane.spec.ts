import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
	AUTO_READ_MS,
	listOwnsAct,
	listTarget,
	scheduleAutoRead,
	splitShown,
	threadRouteId
} from './readingPane';

const url = (href: string) => new URL(href, 'http://tagmail.invalid');
const none = () => null;

describe('threadRouteId', () => {
	it('reads the id of /thread/<id> only', () => {
		expect(threadRouteId(url('/thread/42'))).toBe(42);
		expect(threadRouteId(url('/thread/x'))).toBeNull();
		expect(threadRouteId(url('/inbox'))).toBeNull();
	});
});

describe('splitShown', () => {
	it('splits threads and lists when the pane is on', () => {
		expect(splitShown(true, url('/thread/1'))).toBe(true);
		expect(splitShown(true, url('/inbox'))).toBe(true);
		expect(splitShown(true, url('/search?q=hi'))).toBe(true);
	});

	it('leaves other pages and the pane-off case alone', () => {
		expect(splitShown(false, url('/thread/1'))).toBe(false);
		expect(splitShown(false, url('/inbox'))).toBe(false);
		expect(splitShown(true, url('/search'))).toBe(false);
		expect(splitShown(true, url('/settings/accounts'))).toBe(false);
	});
});

describe('listTarget', () => {
	it("uses a list route's own list", () => {
		const current = vi.fn(() => ({ view: 'inbox' as const, path: '/inbox' }));
		const stored = vi.fn(() => '/label/work');
		expect(listTarget(url('/label/x?account=2'), current, stored, null)).toEqual({
			view: 'account:2/label:x',
			path: '/label/x?account=2'
		});
		expect(current).not.toHaveBeenCalled();
		expect(stored).not.toHaveBeenCalled();
	});

	it('keeps the open list on a thread', () => {
		const stored = vi.fn(() => '/label/work');
		expect(
			listTarget(url('/thread/7'), () => ({ view: 'mailbox:3', path: '/mailbox/3' }), stored, null)
		).toEqual({ view: 'mailbox:3', path: '/mailbox/3' });
		expect(stored).not.toHaveBeenCalled();
	});

	it('falls back to the stored path on a cold thread load', () => {
		expect(listTarget(url('/thread/7'), none, () => '/search?q=a%20b', null)).toEqual({
			view: 'search:a b',
			path: '/search?q=a%20b'
		});
	});

	it('falls back to the inbox of the current account', () => {
		expect(listTarget(url('/thread/7'), none, none, null)).toEqual({
			view: 'inbox',
			path: '/inbox'
		});
		expect(listTarget(url('/thread/7'), none, () => '/settings', 4)).toEqual({
			view: 'account:4/inbox',
			path: '/inbox?account=4'
		});
	});
});

describe('listOwnsAct', () => {
	const base = { checked: 0, selectedId: 5, routeId: 5, focusInList: false };

	it('lets the conversation act by default', () => {
		expect(listOwnsAct(base)).toBe(false);
		expect(listOwnsAct({ ...base, focusInList: true })).toBe(false);
		expect(listOwnsAct({ ...base, selectedId: 6 })).toBe(false);
	});

	it('lets the list act on ticked rows or a different highlight it holds focus for', () => {
		expect(listOwnsAct({ ...base, checked: 2 })).toBe(true);
		expect(listOwnsAct({ ...base, selectedId: 6, focusInList: true })).toBe(true);
	});
});

describe('scheduleAutoRead', () => {
	beforeEach(() => vi.useFakeTimers());
	afterEach(() => vi.useRealTimers());

	it('marks an unread conversation read after the delay', () => {
		const markRead = vi.fn();
		scheduleAutoRead({ split: true, isUnread: () => true, markRead });
		vi.advanceTimersByTime(AUTO_READ_MS - 1);
		expect(markRead).not.toHaveBeenCalled();
		vi.advanceTimersByTime(1);
		expect(markRead).toHaveBeenCalledOnce();
	});

	it('is cancelled by leaving early', () => {
		const markRead = vi.fn();
		const cancel = scheduleAutoRead({ split: true, isUnread: () => true, markRead });
		vi.advanceTimersByTime(AUTO_READ_MS - 1);
		cancel();
		vi.advanceTimersByTime(AUTO_READ_MS);
		expect(markRead).not.toHaveBeenCalled();
	});

	it('does nothing without a split or for a read conversation', () => {
		const markRead = vi.fn();
		scheduleAutoRead({ split: false, isUnread: () => true, markRead });
		scheduleAutoRead({ split: true, isUnread: () => false, markRead });
		vi.advanceTimersByTime(AUTO_READ_MS * 2);
		expect(markRead).not.toHaveBeenCalled();
	});

	it('skips a conversation read in the meantime', () => {
		const markRead = vi.fn();
		let unread = true;
		scheduleAutoRead({ split: true, isUnread: () => unread, markRead });
		unread = false;
		vi.advanceTimersByTime(AUTO_READ_MS);
		expect(markRead).not.toHaveBeenCalled();
	});
});
