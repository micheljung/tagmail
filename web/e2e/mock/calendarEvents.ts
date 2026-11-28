import type {
	CalEditDto,
	CalendarDto,
	EventDetailDto,
	EventFieldsDto,
	EventInstanceDto
} from '../../../packages/api-types/src/index.ts';
import { ACCOUNT_A } from './accounts.ts';
import { ME } from './fixture.ts';

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
	/** `null`: floating, its times read as UTC; default UTC. */
	tzid?: string | null;
	description?: string;
	attendees?: string[];
	alarms?: number[];
	organizer?: string;
	name?: string;
	sequence?: number;
	etag?: number;
}

/** What `calendarApi`'s configured calendars hold, as their CalDAV sync stores it. */
const INITIAL: MockEvent[] = [
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
		uid: 'workshop',
		summary: 'Workshop',
		start: utc(2026, 12, 4, 9),
		end: utc(2026, 12, 4, 11),
		organizer: 'boss@example.com',
		attendees: [ME.email]
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

let EVENTS: MockEvent[] = [];
let etags = 0;

export function resetEvents(): void {
	EVENTS = INITIAL.map((e) => ({ ...e }));
}
resetEvents();

function stamp(t: number): string {
	return new Date(t * 1000).toISOString().replace(/[-:]|\.\d+/g, '');
}

function instance(e: MockEvent, start: number, recurrenceId: string | null): EventInstanceDto {
	return {
		accountId: ACCOUNT_A,
		calendarHref: e.calendarHref,
		href: hrefOf(e),
		uid: e.uid,
		recurrenceId,
		start,
		end: start + e.end - e.start,
		allDay: e.allDay ?? false,
		summary: e.summary,
		location: e.location ?? null,
		status: e.status ?? null,
		partstat: null,
		floating: e.tzid === null
	};
}

const hrefOf = (e: MockEvent) => `${e.calendarHref}${e.name ?? e.uid}.ics`;

/** Offset of `zone` from UTC at Unix seconds `t`, in seconds. */
function offset(zone: string, t: number): number {
	const parts = new Intl.DateTimeFormat('en-US', {
		timeZone: zone,
		hourCycle: 'h23',
		year: 'numeric',
		month: 'numeric',
		day: 'numeric',
		hour: 'numeric',
		minute: 'numeric',
		second: 'numeric'
	}).formatToParts(new Date(t * 1000));
	const n = (type: string) => Number(parts.find((p) => p.type === type)!.value);
	return (
		Date.UTC(n('year'), n('month') - 1, n('day'), n('hour'), n('minute'), n('second')) / 1000 - t
	);
}

const iso = (t: number) => new Date(t * 1000).toISOString().slice(0, 19);

function wallText(t: number, e: Pick<MockEvent, 'allDay' | 'tzid'>): string {
	if (e.allDay) return iso(t).slice(0, 10);
	const zone = e.tzid === undefined ? 'UTC' : e.tzid;
	return iso(zone === null || zone === 'UTC' ? t : t + offset(zone, t));
}

/** Unix seconds of a wall clock `text` in `zone` (`null` floating: read as UTC). */
function fromWall(text: string, allDay: boolean, zone: string | null): number {
	const wall = Date.parse(`${allDay ? `${text}T00:00:00` : text}Z`) / 1000;
	if (allDay || zone === null || zone === 'UTC') return wall;
	const guess = wall - offset(zone, wall);
	return wall - offset(zone, guess);
}

const MINE = [ME.email, 'michel@work.example'];
const organizedByMe = (e: MockEvent) => !e.organizer || MINE.includes(e.organizer);

export function eventDetail(accountId: number, href: string): EventDetailDto | undefined {
	const e = EVENTS.find((x) => accountId === ACCOUNT_A && hrefOf(x) === href);
	if (!e) return undefined;
	const fields: EventFieldsDto = {
		summary: e.summary,
		location: e.location ?? null,
		description: e.description ?? null,
		start: wallText(e.start, e),
		end: wallText(e.end, e),
		allDay: e.allDay ?? false,
		tzid: e.allDay ? null : e.tzid === undefined ? 'UTC' : e.tzid,
		attendees: (e.attendees ?? []).map((email) => ({
			email,
			name: null,
			partstat: 'needs_action'
		})),
		alarms: e.alarms ?? []
	};
	return {
		accountId,
		calendarHref: e.calendarHref,
		href,
		etag: `"${e.etag ?? 0}"`,
		uid: e.uid,
		sequence: e.sequence ?? 0,
		organizer: e.organizer ? { name: null, email: e.organizer, altEmail: null } : null,
		organizedByMe: organizedByMe(e),
		recurring: e.weekly ?? false,
		fixedTimes: false,
		fields
	};
}

/** What `POST /api/calendar/ops` writes once its op runs, as the server checks it. */
export type EditCheck =
	{ field: string; message: string } | { conflict: string } | { uid: string; apply: () => void };

export function checkEdit(
	calendars: readonly CalendarDto[],
	body: CalEditDto,
	me: string
): EditCheck {
	const writable = (href: string | undefined) =>
		calendars.some((c) => c.accountId === body.accountId && c.href === href && c.writable);
	if (!writable(body.calendarHref))
		return { field: 'calendarHref', message: 'no such writable calendar' };
	const existing = body.href ? EVENTS.find((e) => hrefOf(e) === body.href) : undefined;
	if (body.href && !existing) return { conflict: 'no such event' };
	if (existing && body.etag && body.etag !== `"${existing.etag ?? 0}"`) {
		return { conflict: 'the event changed since it was opened; open it again' };
	}
	if (body.kind === 'delete') {
		return {
			uid: existing!.uid,
			apply: () => void (EVENTS = EVENTS.filter((e) => e !== existing))
		};
	}
	if (body.kind === 'move') {
		if (!writable(body.destCalendarHref) || body.destCalendarHref === body.calendarHref) {
			return { field: 'destCalendarHref', message: 'another calendar of the account' };
		}
		return {
			uid: existing!.uid,
			apply: () => {
				existing!.calendarHref = body.destCalendarHref!;
				existing!.etag = ++etags;
			}
		};
	}
	const f = body.event;
	if (!f) return { field: 'event', message: 'a put needs the event' };
	const zone = f.allDay ? null : f.tzid;
	const start = fromWall(f.start, f.allDay, zone);
	const end = fromWall(f.end, f.allDay, zone);
	if (Number.isNaN(start)) return { field: 'start', message: 'not a date or wall clock time' };
	if (Number.isNaN(end)) return { field: 'end', message: 'not a date or wall clock time' };
	if (f.allDay ? end <= start : end < start) {
		return { field: 'end', message: 'the end must not be before the start' };
	}
	const guests = f.attendees.map((a) => a.email.toLowerCase()).sort();
	const held = (existing?.attendees ?? []).map((a) => a.toLowerCase()).sort();
	if (existing && !organizedByMe(existing) && guests.join() !== held.join()) {
		return { field: 'attendees', message: 'the organizer decides who is invited to this event' };
	}
	const next: MockEvent = {
		...(existing ?? { calendarHref: body.calendarHref, uid: `new-${++etags}` }),
		summary: f.summary ?? '',
		location: f.location ?? undefined,
		description: f.description ?? undefined,
		start,
		end,
		allDay: f.allDay,
		tzid: f.allDay ? undefined : f.tzid,
		attendees: f.attendees.map((a) => a.email),
		alarms: f.alarms
	};
	if (next.attendees!.length > 0 && !next.organizer) next.organizer = me;
	const seen = (e: MockEvent) =>
		JSON.stringify({ ...e, alarms: undefined, etag: undefined, sequence: undefined });
	const significant = !existing || seen(existing) !== seen(next);
	return {
		uid: next.uid,
		apply: () => {
			next.etag = ++etags;
			next.sequence = (existing?.sequence ?? 0) + (existing && significant ? 1 : 0);
			if (existing) Object.assign(existing, next);
			else EVENTS.push(next);
		}
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
