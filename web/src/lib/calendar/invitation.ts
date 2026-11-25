import type {
	InvitationEventDto,
	InvitationMethodDto,
	RecurrenceDto,
	SenderAuthDto,
	WeekdayNumDto
} from './types';

export const ATTENDEE_COLLAPSE_LIMIT = 5;

const DAY_MS = 86_400_000;
const UTC = 'UTC';

export interface When {
	primary: string;
	original: string | null;
}

export interface WhenOptions {
	locale?: string;
	viewerZone?: string;
}

export function viewerZone(): string {
	return Intl.DateTimeFormat().resolvedOptions().timeZone;
}

function dateOnly(value: string): Date {
	return new Date(`${value}T00:00:00Z`);
}

function dayText(d: Date, zone: string, locale?: string): string {
	return d.toLocaleDateString(locale, {
		weekday: 'short',
		month: 'short',
		day: 'numeric',
		year: 'numeric',
		timeZone: zone
	});
}

function timeText(d: Date, zone: string, locale?: string): string {
	return d.toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit', timeZone: zone });
}

function sameDay(a: Date, b: Date, zone: string): boolean {
	return dayText(a, zone) === dayText(b, zone);
}

function range(start: Date, end: Date | null, zone: string, locale?: string): string {
	const from = `${dayText(start, zone, locale)}, ${timeText(start, zone, locale)}`;
	if (!end || end.getTime() === start.getTime()) return from;
	if (sameDay(start, end, zone)) return `${from} – ${timeText(end, zone, locale)}`;
	return `${from} – ${dayText(end, zone, locale)}, ${timeText(end, zone, locale)}`;
}

function validZone(zone: string): boolean {
	try {
		new Intl.DateTimeFormat(undefined, { timeZone: zone });
		return true;
	} catch {
		return false;
	}
}

const FLOATING = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(:\d{2})?$/;

export function isFloating(value: string): boolean {
	return FLOATING.test(value);
}

/** A floating time is wall-clock time: read it as UTC and print it as UTC, never converted. */
function floatingWhen(event: InvitationEventDto, locale?: string): When {
	const start = new Date(`${event.start}Z`);
	const end = new Date(`${event.end}Z`);
	return { primary: range(start, end, UTC, locale), original: event.tzid };
}

function allDayWhen(event: InvitationEventDto, locale?: string): When {
	const start = dateOnly(event.start);
	const lastDay = new Date(dateOnly(event.end).getTime() - DAY_MS);
	const label =
		lastDay.getTime() <= start.getTime()
			? dayText(start, UTC, locale)
			: `${dayText(start, UTC, locale)} – ${dayText(lastDay, UTC, locale)}`;
	return { primary: `${label} · All day`, original: null };
}

export function eventWhen(event: InvitationEventDto, options: WhenOptions = {}): When {
	if (event.allDay) return allDayWhen(event, options.locale);
	if (isFloating(event.start)) return floatingWhen(event, options.locale);
	const zone = options.viewerZone ?? viewerZone();
	const start = new Date(event.start);
	const end = new Date(event.end);
	const primary = range(start, end, zone, options.locale);
	const tzid = event.tzid;
	if (!tzid || tzid === zone) return { primary, original: null };
	if (!validZone(tzid)) return { primary, original: tzid };
	return { primary, original: `${range(start, end, tzid, options.locale)} (${tzid})` };
}

const FREQUENCIES: Partial<Record<RecurrenceDto['frequency'], [string, string]>> = {
	daily: ['day', 'days'],
	weekly: ['week', 'weeks'],
	monthly: ['month', 'months'],
	yearly: ['year', 'years']
};

const WEEKDAYS: Record<WeekdayNumDto['weekday'], string> = {
	mo: 'Monday',
	tu: 'Tuesday',
	we: 'Wednesday',
	th: 'Thursday',
	fr: 'Friday',
	sa: 'Saturday',
	su: 'Sunday'
};

function ordinal(n: number): string {
	if (n === -1) return 'last';
	if (n < 0) return `${ordinal(-n)} to last`;
	const suffix = ['th', 'st', 'nd', 'rd'];
	const v = n % 100;
	return `${n}${suffix[(v - 20) % 10] ?? suffix[v] ?? suffix[0]}`;
}

function weekday(d: WeekdayNumDto): string {
	return d.ordinal === null ? WEEKDAYS[d.weekday] : `${ordinal(d.ordinal)} ${WEEKDAYS[d.weekday]}`;
}

function untilText(until: string, locale?: string): string {
	return isFloating(until) || until.endsWith('Z')
		? dayText(new Date(until.endsWith('Z') ? until : `${until}Z`), UTC, locale)
		: dayText(dateOnly(until), UTC, locale);
}

/** "Every 2 weeks on Monday, Wednesday, 10 times"; `null` for a frequency finer than daily. */
export function recurrenceSummary(rule: RecurrenceDto | null, locale?: string): string | null {
	if (!rule) return null;
	const unit = FREQUENCIES[rule.frequency];
	if (!unit) return null;
	let text = rule.interval > 1 ? `Every ${rule.interval} ${unit[1]}` : `Every ${unit[0]}`;
	if (rule.byDay.length) text += ` on ${rule.byDay.map(weekday).join(', ')}`;
	if (rule.byMonthDay.length) text += ` on day ${rule.byMonthDay.join(', ')}`;
	if (rule.count !== null) text += `, ${rule.count} ${rule.count === 1 ? 'time' : 'times'}`;
	else if (rule.until) text += `, until ${untilText(rule.until, locale)}`;
	return text;
}

export type Segment = { text: string; href?: undefined } | { text: string; href: string };

const URL_PATTERN = /https?:\/\/[^\s<>"']+/gi;
const TRAILING_PUNCTUATION = /[.,;:!?)\]]+$/;

/** Splits plain text into text and http(s) link segments; nothing here is ever HTML. */
export function linkify(text: string): Segment[] {
	const out: Segment[] = [];
	let last = 0;
	for (const m of text.matchAll(URL_PATTERN)) {
		const raw = m[0];
		const url = raw.replace(TRAILING_PUNCTUATION, '');
		if (!isHttpUrl(url)) continue;
		if (m.index > last) out.push({ text: text.slice(last, m.index) });
		out.push({ text: url, href: url });
		last = m.index + url.length;
	}
	if (last < text.length) out.push({ text: text.slice(last) });
	return out;
}

export function isHttpUrl(value: string): boolean {
	try {
		const { protocol } = new URL(value);
		return protocol === 'http:' || protocol === 'https:';
	} catch {
		return false;
	}
}

export function senderWarning(method: InvitationMethodDto, auth: SenderAuthDto): string | null {
	if (auth === 'address_mismatch') {
		return method === 'reply'
			? 'The sender is not this attendee.'
			: 'The sender is not the organizer.';
	}
	if (auth === 'unauthenticated') return 'Sender not verified.';
	return null;
}

export function visibleAttendees<T>(attendees: readonly T[], expanded: boolean): T[] {
	return expanded ? [...attendees] : attendees.slice(0, ATTENDEE_COLLAPSE_LIMIT);
}
