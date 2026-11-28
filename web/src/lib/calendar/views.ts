import type { CalendarDto, CalendarTargetDto, EventInstanceDto } from '@tagmail/api-types';

export type CalendarView = 'agenda' | 'day' | 'week' | 'month';

export const VIEWS: readonly CalendarView[] = ['agenda', 'day', 'week', 'month'];
export const AGENDA_DAYS = 30;
const MONTH_CELLS = 42;

export function parseView(value: string | null): CalendarView {
	return VIEWS.find((v) => v === value) ?? 'week';
}

/** Local midnight of `YYYY-MM-DD`, else of `today`. */
export function parseDate(value: string | null, today: Date): Date {
	const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value ?? '');
	const d = m && new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
	return d && dayKey(d) === value ? d : startOfDay(today);
}

export function dayKey(d: Date): string {
	const pad = (n: number) => String(n).padStart(2, '0');
	return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** Minutes on the wall clock since midnight, so DST days keep their hours in place. */
export function wallMinutes(d: Date): number {
	return d.getHours() * 60 + d.getMinutes() + d.getSeconds() / 60;
}

/** A key unique to one instance, across accounts and resources. */
export function instanceKey(e: EventInstanceDto): string {
	return JSON.stringify([e.accountId, e.calendarHref, e.href, e.uid, e.recurrenceId, e.start]);
}

export function startOfDay(d: Date): Date {
	return new Date(d.getFullYear(), d.getMonth(), d.getDate());
}

export function addDays(d: Date, n: number): Date {
	return new Date(d.getFullYear(), d.getMonth(), d.getDate() + n);
}

/** Weeks start on Monday. */
export function startOfWeek(d: Date): Date {
	return addDays(d, -((d.getDay() + 6) % 7));
}

/** The days a view shows around `date`. */
export function viewDays(view: CalendarView, date: Date): Date[] {
	const first = {
		agenda: date,
		day: date,
		week: startOfWeek(date),
		month: startOfWeek(new Date(date.getFullYear(), date.getMonth(), 1))
	}[view];
	const count = { agenda: AGENDA_DAYS, day: 1, week: 7, month: MONTH_CELLS }[view];
	return Array.from({ length: count }, (_, i) => addDays(first, i));
}

/** The date one page before (`-1`) or after (`1`) `date`. */
export function step(view: CalendarView, date: Date, dir: -1 | 1): Date {
	if (view === 'month') return new Date(date.getFullYear(), date.getMonth() + dir, 1);
	return addDays(date, dir * { agenda: AGENDA_DAYS, day: 1, week: 7 }[view]);
}

/** The calendars to ask for: none means all; `null` when every calendar is hidden. */
export function shownCalendars(calendars: readonly CalendarDto[]): CalendarTargetDto[] | null {
	const shown = calendars.filter((c) => c.visible);
	if (shown.length === calendars.length) return [];
	if (shown.length === 0) return null;
	return shown.map((c) => ({ accountId: c.accountId, calendarHref: c.href }));
}

/**
 * The days an instance covers. All-day instances carry their dates as UTC
 * midnights with an exclusive end; timed ones cover each local day they
 * overlap, and one without duration its start day.
 */
export function eventDays(e: EventInstanceDto): string[] {
	const keys: string[] = [];
	if (e.allDay) {
		const at = (t: number) => {
			const d = new Date(t * 1000);
			return new Date(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate());
		};
		const end = at(Math.max(e.end, e.start + 86_400));
		for (let d = at(e.start); d < end; d = addDays(d, 1)) keys.push(dayKey(d));
		return keys;
	}
	const end = new Date(Math.max(e.end, e.start + 1) * 1000);
	for (let d = startOfDay(new Date(e.start * 1000)); d < end; d = addDays(d, 1)) {
		keys.push(dayKey(d));
	}
	return keys;
}

export function byDay(events: readonly EventInstanceDto[]): Map<string, EventInstanceDto[]> {
	const days = new Map<string, EventInstanceDto[]>();
	for (const e of events) {
		for (const key of eventDays(e)) {
			const list = days.get(key) ?? [];
			list.push(e);
			days.set(key, list);
		}
	}
	for (const list of days.values()) list.sort((a, b) => Number(b.allDay) - Number(a.allDay));
	return days;
}

export interface Placed {
	event: EventInstanceDto;
	/** Minutes after midnight, clipped to the day. */
	top: number;
	height: number;
	lane: number;
	lanes: number;
}

/** Timed instances of `day` in side-by-side lanes where they overlap. */
export function placeDay(events: readonly EventInstanceDto[], day: Date): Placed[] {
	const start = day.getTime() / 1000;
	const end = addDays(day, 1).getTime() / 1000;
	const placed: Placed[] = events
		.filter((e) => !e.allDay && e.start < end && (e.end > start || e.start >= start))
		.map((e) => {
			const top = e.start <= start ? 0 : wallMinutes(new Date(e.start * 1000));
			const bottom = e.end >= end ? 24 * 60 : wallMinutes(new Date(e.end * 1000));
			const lasts = Math.min((e.end - e.start) / 60, 24 * 60 - top);
			return { event: e, top, height: Math.max(bottom - top, lasts, 0), lane: 0, lanes: 1 };
		})
		.sort((a, b) => a.top - b.top || b.height - a.height);
	let cluster: Placed[] = [];
	let clusterEnd = -1;
	const close = () => {
		const lanes = Math.max(1, ...cluster.map((p) => p.lane + 1));
		for (const p of cluster) p.lanes = lanes;
		cluster = [];
	};
	for (const p of placed) {
		const bottom = Math.max(p.top + p.height, p.top + 15);
		if (p.top >= clusterEnd) close();
		const busy = new Set(
			cluster.filter((q) => Math.max(q.top + q.height, q.top + 15) > p.top).map((q) => q.lane)
		);
		while (busy.has(p.lane)) p.lane++;
		cluster.push(p);
		clusterEnd = Math.max(clusterEnd, bottom);
	}
	close();
	return placed;
}

/** Local time of the same wall clock as the UTC reading of Unix seconds `t`. */
function wallClock(t: number): number {
	const d = new Date(t * 1000);
	const local = new Date(
		d.getUTCFullYear(),
		d.getUTCMonth(),
		d.getUTCDate(),
		d.getUTCHours(),
		d.getUTCMinutes(),
		d.getUTCSeconds()
	);
	return local.getTime() / 1000;
}

/** Floating instances carry their wall clock as UTC; shown, it is local time. */
export function asShown(events: readonly EventInstanceDto[]): EventInstanceDto[] {
	return events.map((e) =>
		e.floating ? { ...e, start: wallClock(e.start), end: wallClock(e.end) } : e
	);
}

/** The calendar's colour for CSS; the accent when it has none. */
export function calendarColor(c: CalendarDto): string {
	return c.color ?? 'var(--accent)';
}

/** A `<input type="color">` value: `#rrggbb`. */
export function swatchValue(c: CalendarDto): string {
	return /^#[0-9a-f]{6}/i.test(c.color ?? '') ? c.color!.slice(0, 7) : '#2350c8';
}
