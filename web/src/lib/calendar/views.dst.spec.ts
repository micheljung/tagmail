import type { EventInstanceDto } from '@tagmail/api-types';
import { afterAll, beforeAll, describe, expect, it } from 'vitest';
import { placeDay } from './views';

const zone = process.env.TZ;

function instance(startUtc: string, endUtc: string): EventInstanceDto {
	return {
		accountId: 1,
		calendarHref: '/c/',
		href: `/c/${startUtc}.ics`,
		uid: startUtc,
		recurrenceId: null,
		start: Date.parse(startUtc) / 1000,
		end: Date.parse(endUtc) / 1000,
		allDay: false,
		summary: null,
		location: null,
		status: null,
		partstat: null
	};
}

describe('day placement across DST in Europe/Zurich', () => {
	beforeAll(() => {
		process.env.TZ = 'Europe/Zurich';
	});
	afterAll(() => {
		process.env.TZ = zone;
	});

	it('keeps 9:00 at 9:00 on the day clocks go forward', () => {
		const day = new Date(2026, 2, 29);
		const [nine, wholeDay] = placeDay(
			[
				instance('2026-03-29T07:00:00Z', '2026-03-29T08:00:00Z'),
				instance('2026-03-28T23:00:00Z', '2026-03-29T22:00:00Z')
			],
			day
		).sort((a, b) => b.top - a.top);
		expect([nine.top, nine.height]).toEqual([9 * 60, 60]);
		expect([wholeDay.top, wholeDay.height]).toEqual([0, 24 * 60]);
	});

	it('keeps 9:00 at 9:00 on the day clocks go back', () => {
		const day = new Date(2026, 9, 25);
		const [nine] = placeDay([instance('2026-10-25T08:00:00Z', '2026-10-25T09:30:00Z')], day);
		expect([nine.top, nine.height]).toEqual([9 * 60, 90]);
	});
});
