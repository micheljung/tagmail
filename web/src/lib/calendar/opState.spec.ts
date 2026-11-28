import { describe, expect, it } from 'vitest';
import type { CalOpInfoDto } from '../transport';
import { calendarNote, describeCalendarOp } from './opState';

const info = (patch: Partial<CalOpInfoDto> = {}): CalOpInfoDto => ({
	targetAccountId: 1,
	calendarHref: '/dav/personal/',
	uid: 'standup-1',
	conflict: null,
	replyOpId: null,
	reply: null,
	...patch
});

describe('describeCalendarOp', () => {
	it('names the kind and the event uid', () => {
		expect(describeCalendarOp('cal.rsvp', info())).toBe('Reply to invitation standup-1');
		expect(describeCalendarOp('cal.import', info())).toBe('Add event standup-1 to calendar');
		expect(describeCalendarOp('cal.apply', info())).toBe('Update event standup-1 in calendar');
	});
});

describe('calendarNote', () => {
	it('explains a conflict and offers Re-apply', () => {
		expect(calendarNote('failed', info({ conflict: 'server_won' }))).toEqual({
			text: "Calendar changed on the server — your edit wasn't saved",
			retryLabel: 'Re-apply'
		});
	});

	it('says the guests were not told when an invitation waits on a rejected edit', () => {
		expect(
			calendarNote('failed', info({ conflict: 'server_won', replyOpId: 'e-1#request' }))?.text
		).toBe("Calendar changed on the server — your edit wasn't saved and guests were not told");
	});

	it('says guests were not told when a failed edit holds its invitations', () => {
		const held = info({ replyOpId: 'e-1#cancel', reply: 'queued' });
		expect(calendarNote('failed', held)?.text).toBe(
			"Your edit wasn't saved and guests were not told"
		);
		expect(calendarNote('done', { ...held, reply: 'cancelled' })?.text).toBe(
			'Guests were not told about this change'
		);
	});

	it('says the reply went out when only the calendar write failed', () => {
		expect(calendarNote('failed', info({ reply: 'sent' }))?.text).toBe(
			'Reply sent, not saved to calendar'
		);
	});

	it('says the reply did not go out when it was cancelled', () => {
		expect(calendarNote('failed', info({ reply: 'cancelled' }))?.text).toBe('Reply not sent');
	});

	it('has nothing to add to a queued reply or a sent one that was saved', () => {
		expect(calendarNote('pending', info({ reply: 'queued' }))).toBeNull();
		expect(calendarNote('pending', info({ reply: 'sent' }))).toBeNull();
	});
});
