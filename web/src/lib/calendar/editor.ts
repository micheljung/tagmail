import type { EventFieldsDto } from '@tagmail/api-types';
import { addDays, dayKey } from './views';

/** What the editor opens: a new event at a time, or a stored one. */
export type EditorTarget =
	{ kind: 'new'; start: Date; allDay: boolean } | { kind: 'edit'; accountId: number; href: string };

/** What the editor form holds. All-day events show their last day, not the exclusive end. */
export interface Draft {
	summary: string;
	location: string;
	description: string;
	allDay: boolean;
	startDate: string;
	startTime: string;
	endDate: string;
	endTime: string;
	/** `null`: floating. */
	tzid: string | null;
	attendees: string[];
	alarms: number[];
}

export const UTC = 'UTC';
const DEFAULT_MINUTES = 60;
const SLOT_MINUTES = 30;

export const REMINDERS: readonly { minutes: number; label: string }[] = [
	{ minutes: 0, label: 'At start' },
	{ minutes: 5, label: '5 minutes before' },
	{ minutes: 10, label: '10 minutes before' },
	{ minutes: 15, label: '15 minutes before' },
	{ minutes: 30, label: '30 minutes before' },
	{ minutes: 60, label: '1 hour before' },
	{ minutes: 120, label: '2 hours before' },
	{ minutes: 1440, label: '1 day before' },
	{ minutes: 2880, label: '2 days before' },
	{ minutes: 10080, label: '1 week before' }
];

export function reminderLabel(minutes: number): string {
	return REMINDERS.find((r) => r.minutes === minutes)?.label ?? `${minutes} minutes before`;
}

const pad = (n: number) => String(n).padStart(2, '0');

function parseDay(key: string): Date {
	const [y, m, d] = key.split('-').map(Number);
	return new Date(y, m - 1, d);
}

export function browserZone(): string {
	return Intl.DateTimeFormat().resolvedOptions().timeZone || UTC;
}

/** The time zones to offer, with `current` (which may be a Windows name) kept. */
export function zones(current: string | null): string[] {
	const known =
		typeof Intl.supportedValuesOf === 'function' ? Intl.supportedValuesOf('timeZone') : [];
	const list = [UTC, ...known.filter((z) => z !== UTC)];
	return current && !list.includes(current) ? [current, ...list] : list;
}

/** A new event at `start` in the browser's zone; `allDay` for the whole day. */
export function newDraft(start: Date, allDay: boolean, tzid = browserZone()): Draft {
	const end = new Date(start.getTime() + DEFAULT_MINUTES * 60_000);
	const clock = (d: Date) => `${pad(d.getHours())}:${pad(d.getMinutes())}`;
	return {
		summary: '',
		location: '',
		description: '',
		allDay,
		startDate: dayKey(start),
		startTime: allDay ? '09:00' : clock(start),
		endDate: allDay ? dayKey(start) : dayKey(end),
		endTime: allDay ? '10:00' : clock(end),
		tzid,
		attendees: [],
		alarms: []
	};
}

/** The slot a click `minutes` after midnight of `day` lands in. */
export function slotStart(day: Date, minutes: number): Date {
	const slot = Math.max(0, Math.floor(minutes / SLOT_MINUTES) * SLOT_MINUTES);
	return new Date(
		day.getFullYear(),
		day.getMonth(),
		day.getDate(),
		0,
		Math.min(slot, 23 * 60 + 30)
	);
}

export function draftOf(f: EventFieldsDto): Draft {
	const [startDate, startClock = ''] = f.start.split('T');
	const [endDate, endClock = ''] = f.end.split('T');
	return {
		summary: f.summary ?? '',
		location: f.location ?? '',
		description: f.description ?? '',
		allDay: f.allDay,
		startDate,
		startTime: f.allDay ? '09:00' : startClock.slice(0, 5),
		endDate: f.allDay ? dayKey(addDays(parseDay(endDate), -1)) : endDate,
		endTime: f.allDay ? '10:00' : endClock.slice(0, 5),
		tzid: f.allDay ? browserZone() : f.tzid,
		attendees: f.attendees.map((a) => a.email),
		alarms: [...f.alarms]
	};
}

const wall = (date: string, time: string) => Date.parse(`${date}T${time || '00:00'}:00Z`);

/** The draft starting at `date` and `time` and lasting as long as before. */
export function withStart(d: Draft, date: string, time: string): Draft {
	const lasts = wall(d.endDate, d.endTime) - wall(d.startDate, d.startTime);
	const end = new Date(wall(date, time) + (Number.isNaN(lasts) ? 0 : lasts));
	if (!date || Number.isNaN(end.getTime())) return { ...d, startDate: date, startTime: time };
	const iso = end.toISOString();
	return {
		...d,
		startDate: date,
		startTime: time,
		endDate: iso.slice(0, 10),
		endTime: d.allDay ? d.endTime : iso.slice(11, 16)
	};
}

/** Why the draft cannot be saved; `null` when it can. */
export function problem(d: Draft): string | null {
	if (!d.startDate || !d.endDate || (!d.allDay && (!d.startTime || !d.endTime))) {
		return 'Enter a start and an end.';
	}
	const start = d.allDay ? d.startDate : `${d.startDate}T${d.startTime}`;
	const end = d.allDay ? d.endDate : `${d.endDate}T${d.endTime}`;
	if (end < start) return 'The event ends before it starts.';
	const bad = d.attendees.find((a) => !/^[^\s@<>:;,"]+@[^\s@<>:;,"]+$/.test(a));
	return bad ? `${bad} is not a mail address.` : null;
}

/**
 * The fields to send. Unchanged times keep the original's text, so an edit
 * of the title leaves them exactly as stored.
 */
export function fieldsOf(d: Draft, original?: EventFieldsDto): EventFieldsDto {
	const before = original && draftOf(original);
	const same = (keys: (keyof Draft)[]) =>
		before !== undefined && keys.every((k) => before[k] === d[k]);
	const normal = (v: string | null | undefined) => (v ?? '').replace(/\r\n?/g, '\n').trim() || null;
	const text = (v: string, was?: string | null) =>
		original && normal(v) === normal(was) ? (was ?? null) : normal(v);
	const start = d.allDay ? d.startDate : `${d.startDate}T${d.startTime}:00`;
	const end = d.allDay ? dayKey(addDays(parseDay(d.endDate), 1)) : `${d.endDate}T${d.endTime}:00`;
	const times = same(['allDay', 'startDate', 'startTime', 'endDate', 'endTime', 'tzid']);
	return {
		summary: text(d.summary, original?.summary),
		location: text(d.location, original?.location),
		description: text(d.description, original?.description),
		start: times && original ? original.start : start,
		end: times && original ? original.end : end,
		allDay: d.allDay,
		tzid: d.allDay ? null : d.tzid,
		attendees: d.attendees.map((email) => ({
			email,
			name: original?.attendees.find((a) => a.email === email)?.name ?? null
		})),
		alarms: [...d.alarms]
	};
}

/** Typed addresses, split at commas, semicolons and line breaks; `Name <a@b>` gives `a@b`. */
export function addresses(text: string): string[] {
	return text
		.split(/[,;\n]+/)
		.map((a) => (/<([^>]*)>/.exec(a)?.[1] ?? a).trim())
		.filter(Boolean);
}
