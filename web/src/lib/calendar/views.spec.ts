import type { CalendarDto, EventInstanceDto } from '@tagmail/api-types';
import { describe, expect, it } from 'vitest';
import {
	byDay,
	dayKey,
	eventDays,
	parseDate,
	parseView,
	placeDay,
	shownCalendars,
	step,
	viewDays
} from './views';

const at = (y: number, m: number, d: number, h = 0, min = 0) =>
	new Date(y, m - 1, d, h, min).getTime() / 1000;

function instance(start: number, end: number, extra: Partial<EventInstanceDto> = {}) {
	return {
		accountId: 1,
		calendarHref: '/c/',
		href: `/c/${start}.ics`,
		uid: String(start),
		recurrenceId: null,
		start,
		end,
		allDay: false,
		summary: 'e',
		location: null,
		status: null,
		partstat: null,
		...extra
	} satisfies EventInstanceDto;
}

describe('calendar views', () => {
	it('reads the view and date from the URL', () => {
		const today = new Date(2026, 10, 28, 15);
		expect(parseView('month')).toBe('month');
		expect(parseView('year')).toBe('week');
		expect(dayKey(parseDate('2026-02-03', today))).toBe('2026-02-03');
		expect(dayKey(parseDate('soon', today))).toBe('2026-11-28');
		expect(dayKey(parseDate('2026-13-45', today))).toBe('2026-11-28');
		expect(dayKey(parseDate('2026-02-30', today))).toBe('2026-11-28');
	});

	it('pages through days, weeks from Monday and month grids', () => {
		const wed = new Date(2026, 10, 25);
		expect(viewDays('day', wed).map(dayKey)).toEqual(['2026-11-25']);
		const week = viewDays('week', wed).map(dayKey);
		expect([week[0], week[6]]).toEqual(['2026-11-23', '2026-11-29']);
		const month = viewDays('month', wed);
		expect(month).toHaveLength(42);
		expect([dayKey(month[0]), dayKey(month[41])]).toEqual(['2026-10-26', '2026-12-06']);
		expect(viewDays('agenda', wed)).toHaveLength(30);
		expect(dayKey(step('week', wed, -1))).toBe('2026-11-18');
		expect(dayKey(step('month', wed, 1))).toBe('2026-12-01');
		expect(dayKey(step('agenda', wed, 1))).toBe('2026-12-25');
	});

	it('asks for every calendar, only the shown ones, or none', () => {
		const cal = (href: string, visible: boolean) =>
			({ accountId: 2, href, visible }) as CalendarDto;
		expect(shownCalendars([cal('/a/', true), cal('/b/', true)])).toEqual([]);
		expect(shownCalendars([cal('/a/', true), cal('/b/', false)])).toEqual([
			{ accountId: 2, calendarHref: '/a/' }
		]);
		expect(shownCalendars([cal('/a/', false)])).toBeNull();
	});

	it('spreads all-day dates and overnight events over their days', () => {
		const offsite = instance(Date.UTC(2026, 10, 26) / 1000, Date.UTC(2026, 10, 28) / 1000, {
			allDay: true
		});
		expect(eventDays(offsite)).toEqual(['2026-11-26', '2026-11-27']);
		const overnight = instance(at(2026, 11, 26, 22), at(2026, 11, 27, 2));
		expect(eventDays(overnight)).toEqual(['2026-11-26', '2026-11-27']);
		const untilMidnight = instance(at(2026, 11, 26, 22), at(2026, 11, 27));
		expect(eventDays(untilMidnight)).toEqual(['2026-11-26']);
		const days = byDay([overnight, offsite]);
		expect(days.get('2026-11-26')?.[0]).toBe(offsite);
	});

	it('puts overlapping events side by side and clips them to the day', () => {
		const day = new Date(2026, 10, 25);
		const review = instance(at(2026, 11, 25, 14), at(2026, 11, 25, 15, 30));
		const call = instance(at(2026, 11, 25, 14, 30), at(2026, 11, 25, 15));
		const later = instance(at(2026, 11, 25, 16), at(2026, 11, 25, 17));
		const night = instance(at(2026, 11, 25, 23), at(2026, 11, 26, 1));
		const placed = placeDay([later, call, review, night], day);
		const of = (e: EventInstanceDto) => placed.find((p) => p.event === e)!;
		expect([of(review).lane, of(review).lanes, of(review).top]).toEqual([0, 2, 14 * 60]);
		expect([of(call).lane, of(call).lanes]).toEqual([1, 2]);
		expect([of(later).lane, of(later).lanes]).toEqual([0, 1]);
		expect(of(night).height).toBe(60);
		expect(placeDay([night], new Date(2026, 10, 26))[0].top).toBe(0);
	});
});
