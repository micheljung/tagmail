import { describe, expect, it } from 'vitest';
import {
	ATTENDEE_COLLAPSE_LIMIT,
	eventWhen,
	isHttpUrl,
	linkify,
	recurrenceSummary,
	senderWarning,
	visibleAttendees
} from './invitation';
import type { InvitationEventDto, RecurrenceDto } from './types';

const event = (change: Partial<InvitationEventDto>): InvitationEventDto => ({
	uid: 'u1',
	summary: 'Sync',
	start: '2026-12-04T08:00:00Z',
	end: '2026-12-04T09:00:00Z',
	allDay: false,
	tzid: null,
	location: null,
	description: null,
	url: null,
	organizer: null,
	attendees: [],
	recurrence: null,
	cancelled: false,
	recurrenceId: null,
	...change
});

const rule = (change: Partial<RecurrenceDto>): RecurrenceDto => ({
	frequency: 'weekly',
	interval: 1,
	count: null,
	until: null,
	byDay: [],
	byMonthDay: [],
	byMonth: [],
	exdates: [],
	...change
});

const NEW_YORK = { locale: 'en-US', viewerZone: 'America/New_York' };

describe('eventWhen', () => {
	it('shows a same-day range in the viewer zone', () => {
		expect(eventWhen(event({}), NEW_YORK).primary).toBe('Fri, Dec 4, 2026, 3:00 AM – 4:00 AM');
	});

	it('shows both days for a range across midnight', () => {
		const when = eventWhen(event({ end: '2026-12-05T09:00:00Z' }), NEW_YORK);
		expect(when.primary).toBe('Fri, Dec 4, 2026, 3:00 AM – Sat, Dec 5, 2026, 4:00 AM');
	});

	it('adds the original zone when it differs from the viewer', () => {
		const when = eventWhen(event({ tzid: 'Europe/Zurich' }), NEW_YORK);
		expect(when.original).toBe('Fri, Dec 4, 2026, 9:00 AM – 10:00 AM (Europe/Zurich)');
	});

	it('omits the original zone when it equals the viewer zone', () => {
		expect(eventWhen(event({ tzid: 'America/New_York' }), NEW_YORK).original).toBeNull();
	});

	it('names an unknown zone without converting', () => {
		expect(eventWhen(event({ tzid: 'Customized Time Zone' }), NEW_YORK).original).toBe(
			'Customized Time Zone'
		);
	});

	it('shows a floating time as wall-clock time without conversion', () => {
		const when = eventWhen(
			event({ start: '2026-12-04T09:00:00', end: '2026-12-04T10:00:00', tzid: null }),
			{ locale: 'en-US', viewerZone: 'Asia/Tokyo' }
		);
		expect(when).toEqual({ primary: 'Fri, Dec 4, 2026, 9:00 AM – 10:00 AM', original: null });
	});

	it('names the zone of a floating time without converting it', () => {
		const when = eventWhen(
			event({
				start: '2026-12-04T09:00:00',
				end: '2026-12-04T10:00:00',
				tzid: 'Customized Time Zone'
			}),
			NEW_YORK
		);
		expect(when.original).toBe('Customized Time Zone');
	});

	it('treats an all-day event as a date, not an instant', () => {
		const when = eventWhen(
			event({ allDay: true, start: '2026-12-04', end: '2026-12-05', tzid: 'Asia/Tokyo' }),
			{ locale: 'en-US', viewerZone: 'Pacific/Auckland' }
		);
		expect(when).toEqual({ primary: 'Fri, Dec 4, 2026 · All day', original: null });
	});

	it('spans several all-day days with an exclusive end', () => {
		const when = eventWhen(
			event({ allDay: true, start: '2026-12-04', end: '2026-12-07' }),
			NEW_YORK
		);
		expect(when.primary).toBe('Fri, Dec 4, 2026 – Sun, Dec 6, 2026 · All day');
	});
});

describe('recurrenceSummary', () => {
	it.each([
		[rule({ frequency: 'daily' }), 'Every day'],
		[
			rule({
				byDay: [
					{ ordinal: null, weekday: 'mo' },
					{ ordinal: null, weekday: 'we' }
				]
			}),
			'Every week on Monday, Wednesday'
		],
		[rule({ interval: 2, count: 10 }), 'Every 2 weeks, 10 times'],
		[
			rule({ frequency: 'monthly', byDay: [{ ordinal: 2, weekday: 'tu' }] }),
			'Every month on 2nd Tuesday'
		],
		[
			rule({ frequency: 'monthly', byDay: [{ ordinal: -1, weekday: 'fr' }] }),
			'Every month on last Friday'
		],
		[rule({ frequency: 'monthly', byMonthDay: [15] }), 'Every month on day 15'],
		[
			rule({ frequency: 'yearly', until: '2027-12-31T00:00:00Z' }),
			'Every year, until Fri, Dec 31, 2027'
		],
		[rule({ frequency: 'yearly', until: '2027-12-31' }), 'Every year, until Fri, Dec 31, 2027']
	])('%j', (recurrence, text) => {
		expect(recurrenceSummary(recurrence, 'en-US')).toBe(text);
	});

	it('returns null for no rule or a frequency finer than daily', () => {
		expect(recurrenceSummary(null)).toBeNull();
		expect(recurrenceSummary(rule({ frequency: 'hourly' }))).toBeNull();
	});
});

describe('linkify', () => {
	it('splits text around http(s) links and drops trailing punctuation', () => {
		expect(linkify('Join https://meet.example/x?a=1, then relax.')).toEqual([
			{ text: 'Join ' },
			{ text: 'https://meet.example/x?a=1', href: 'https://meet.example/x?a=1' },
			{ text: ', then relax.' }
		]);
	});

	it('never turns markup or other schemes into links', () => {
		const segments = linkify('<a href="javascript:alert(1)">x</a> javascript:alert(2)');
		expect(segments).toEqual([{ text: '<a href="javascript:alert(1)">x</a> javascript:alert(2)' }]);
	});

	it('returns nothing for empty text', () => {
		expect(linkify('')).toEqual([]);
	});
});

describe('isHttpUrl', () => {
	it.each([
		['https://a.example', true],
		['http://a.example', true],
		['javascript:alert(1)', false],
		['data:text/html,x', false],
		['not a url', false]
	])('%s', (value, expected) => {
		expect(isHttpUrl(value)).toBe(expected);
	});
});

describe('senderWarning', () => {
	it('warns on a mismatched or unauthenticated sender of a REQUEST or CANCEL', () => {
		expect(senderWarning('request', 'address_mismatch')).toBe('The sender is not the organizer.');
		expect(senderWarning('cancel', 'unauthenticated')).toBe('Sender not verified.');
	});

	it('warns when a REPLY comes from someone other than the attendee', () => {
		expect(senderWarning('reply', 'address_mismatch')).toBe('The sender is not this attendee.');
	});

	it('warns for any method, as a mixed message reports other', () => {
		for (const method of ['publish', 'none', 'other'] as const) {
			expect(senderWarning(method, 'address_mismatch')).toBe('The sender is not the organizer.');
			expect(senderWarning(method, 'unauthenticated')).toBe('Sender not verified.');
		}
	});

	it('stays quiet for verified, unconfigured, offline mail', () => {
		for (const auth of ['verified', 'untrusted_config', 'unknown'] as const) {
			expect(senderWarning('request', auth)).toBeNull();
		}
	});
});

describe('visibleAttendees', () => {
	const many = Array.from({ length: ATTENDEE_COLLAPSE_LIMIT + 3 }, (_, i) => i);

	it('collapses to the limit and expands to all', () => {
		expect(visibleAttendees(many, false)).toHaveLength(ATTENDEE_COLLAPSE_LIMIT);
		expect(visibleAttendees(many, true)).toHaveLength(many.length);
	});
});
