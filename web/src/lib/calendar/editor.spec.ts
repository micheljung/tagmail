import type { EventFieldsDto } from '@tagmail/api-types';
import { describe, expect, it } from 'vitest';
import {
	addresses,
	draftOf,
	fieldsOf,
	newDraft,
	problem,
	slotStart,
	withStart,
	zones
} from './editor';

const STORED: EventFieldsDto = {
	summary: 'Planung',
	location: 'Raum 4',
	description: null,
	start: '2026-12-10T09:00:00',
	end: '2026-12-10T10:30:00',
	allDay: false,
	tzid: 'W. Europe Standard Time',
	attendees: [{ email: 'ann@example.com', name: 'Ann', partstat: 'accepted' }],
	alarms: [15]
};

describe('event editor', () => {
	it('sends a title change with the stored times untouched', () => {
		const draft = { ...draftOf(STORED), summary: 'Planning' };
		const fields = fieldsOf(draft, STORED);
		expect(fields).toEqual({
			...STORED,
			summary: 'Planning',
			attendees: [{ email: 'ann@example.com', name: 'Ann' }]
		});
	});

	it('shows an all-day event by its last day and sends the exclusive end', () => {
		const allDay = { ...STORED, allDay: true, start: '2026-12-14', end: '2026-12-16', tzid: null };
		const draft = draftOf(allDay);
		expect([draft.startDate, draft.endDate]).toEqual(['2026-12-14', '2026-12-15']);
		const fields = fieldsOf({ ...draft, endDate: '2026-12-16' }, allDay);
		expect([fields.start, fields.end, fields.tzid]).toEqual(['2026-12-14', '2026-12-17', null]);
	});

	it('starts a new event in the clicked slot, an hour long', () => {
		const at = slotStart(new Date(2026, 11, 1), 9 * 60 + 40);
		const draft = newDraft(at, false, 'Europe/Zurich');
		expect([draft.startDate, draft.startTime, draft.endTime]).toEqual([
			'2026-12-01',
			'09:30',
			'10:30'
		]);
		const fields = fieldsOf({ ...draft, summary: '  Lunch ' });
		expect(fields.summary).toBe('Lunch');
		expect([fields.start, fields.end, fields.tzid]).toEqual([
			'2026-12-01T09:30:00',
			'2026-12-01T10:30:00',
			'Europe/Zurich'
		]);
	});

	it('refuses an end before the start and bad addresses', () => {
		const draft = draftOf(STORED);
		expect(problem(draft)).toBeNull();
		expect(problem({ ...draft, endTime: '08:00' })).toMatch(/before it starts/);
		expect(problem({ ...draft, attendees: ['nobody'] })).toMatch(/not a mail address/);
	});

	it('reads typed addresses and keeps an unknown zone on offer', () => {
		expect(addresses('Ann <ann@example.com>, bo@example.com;\ncy@example.com')).toEqual([
			'ann@example.com',
			'bo@example.com',
			'cy@example.com'
		]);
		expect(zones('W. Europe Standard Time')[0]).toBe('W. Europe Standard Time');
		expect(zones(null)).toContain('UTC');
	});

	it('keeps the length when the start moves', () => {
		const moved = withStart(draftOf(STORED), '2026-12-11', '23:00');
		expect([moved.endDate, moved.endTime]).toEqual(['2026-12-12', '00:30']);
		const allDay = draftOf({ ...STORED, allDay: true, start: '2026-12-14', end: '2026-12-16' });
		const later = withStart(allDay, '2026-12-20', allDay.startTime);
		expect([later.startDate, later.endDate]).toEqual(['2026-12-20', '2026-12-21']);
	});

	it('sends stored text that only differs in white space as stored', () => {
		const padded = { ...STORED, description: 'Agenda\r\n' };
		const fields = fieldsOf({ ...draftOf(padded), description: 'Agenda' }, padded);
		expect(fields.description).toBe('Agenda\r\n');
	});
});
