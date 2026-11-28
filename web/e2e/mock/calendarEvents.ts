import type { CalendarDto, EventInstanceDto } from '../../../packages/api-types/src/index.ts';
import { ACCOUNT_A } from './accounts.ts';

const DAY = 86_400;
const WEEK = 7 * DAY;
const utc = (y: number, m: number, d: number, h = 0, min = 0) =>
	Date.UTC(y, m - 1, d, h, min) / 1000;

export const MAX_EVENTS_SPAN = 62 * DAY;
/** The Unix seconds `time::OffsetDateTime` represents (years -9999 to 9999). */
export const MIN_TIME = -377_705_116_800;
export const MAX_TIME = 253_402_300_799;

interface MockEvent {
	calendarHref: string;
	uid: string;
	summary: string;
	start: number;
	end: number;
	allDay?: boolean;
	location?: string;
	weekly?: boolean;
	status?: EventInstanceDto['status'];
}

/** What `calendarApi`'s configured calendars hold, as their CalDAV sync stores it. */
const EVENTS: MockEvent[] = [
	{
		calendarHref: '/dav/work/',
		uid: 'standup',
		summary: 'Standup',
		start: utc(2026, 1, 5, 8),
		end: utc(2026, 1, 5, 8, 15),
		weekly: true
	},
	{
		calendarHref: '/dav/work/',
		uid: 'review',
		summary: 'Design review',
		start: utc(2026, 11, 25, 13),
		end: utc(2026, 11, 25, 14, 30),
		location: 'Room 4'
	},
	{
		calendarHref: '/dav/personal/',
		uid: 'call',
		summary: 'Call with Sam',
		start: utc(2026, 11, 25, 13, 30),
		end: utc(2026, 11, 25, 14)
	},
	{
		calendarHref: '/dav/personal/',
		uid: 'dentist',
		summary: 'Dentist',
		start: utc(2026, 11, 25, 15),
		end: utc(2026, 11, 25, 16),
		location: 'Main St 1'
	},
	{
		calendarHref: '/dav/work/',
		uid: 'offsite',
		summary: 'Team offsite',
		start: utc(2026, 11, 26),
		end: utc(2026, 11, 28),
		allDay: true
	},
	{
		calendarHref: '/dav/personal/',
		uid: 'dinner',
		summary: 'Dinner',
		start: utc(2026, 11, 27, 18, 30),
		end: utc(2026, 11, 27, 21),
		status: 'cancelled'
	},
	{
		calendarHref: '/dav/personal/',
		uid: 'night-train',
		summary: 'Night train',
		start: utc(2026, 12, 2, 20),
		end: utc(2026, 12, 3, 7)
	},
	{
		calendarHref: '/dav/work/',
		uid: 'dst-review',
		summary: 'Review after the clock change',
		start: utc(2026, 10, 25, 8),
		end: utc(2026, 10, 25, 9)
	},
	{
		calendarHref: '/dav/holidays/',
		uid: 'st-andrew',
		summary: "St Andrew's Day",
		start: utc(2026, 11, 30),
		end: utc(2026, 12, 1),
		allDay: true
	}
];

function stamp(t: number): string {
	return new Date(t * 1000).toISOString().replace(/[-:]|\.\d+/g, '');
}

function instance(e: MockEvent, start: number, recurrenceId: string | null): EventInstanceDto {
	return {
		accountId: ACCOUNT_A,
		calendarHref: e.calendarHref,
		href: `${e.calendarHref}${e.uid}.ics`,
		uid: e.uid,
		recurrenceId,
		start,
		end: start + e.end - e.start,
		allDay: e.allDay ?? false,
		summary: e.summary,
		location: e.location ?? null,
		status: e.status ?? null,
		partstat: null
	};
}

/** `GET /api/calendar/events` over `calendars`: the server's overlap rule and order. */
export function eventsIn(
	calendars: readonly CalendarDto[],
	from: number,
	to: number,
	wanted: readonly { accountId: number; calendarHref: string }[]
): EventInstanceDto[] {
	const listed = (href: string) =>
		calendars.some((c) => c.href === href) &&
		(wanted.length === 0 ||
			wanted.some((w) => w.accountId === ACCOUNT_A && w.calendarHref === href));
	const out: EventInstanceDto[] = [];
	for (const e of EVENTS.filter((e) => listed(e.calendarHref))) {
		const starts = e.weekly
			? Array.from(
					{ length: Math.max(0, Math.ceil((to - e.start) / WEEK)) },
					(_, i) => e.start + i * WEEK
				)
			: [e.start];
		for (const start of starts) {
			const end = start + e.end - e.start;
			if (start < to && (end > from || start >= from)) {
				out.push(instance(e, start, e.weekly ? stamp(start) : null));
			}
		}
	}
	const key = (i: EventInstanceDto) => [
		i.start,
		i.accountId,
		i.calendarHref,
		i.href,
		i.recurrenceId ?? ''
	];
	return out.sort((a, b) => {
		const [x, y] = [key(a), key(b)];
		const at = x.findIndex((v, n) => v !== y[n]);
		return at < 0 ? 0 : x[at] < y[at] ? -1 : 1;
	});
}
