import type { IncomingMessage, ServerResponse } from 'node:http';
import type {
	InvitationAttendeeDto,
	InvitationDto,
	InvitationEventDto
} from '../../src/lib/calendar/types.ts';
import { idAt, messagesOf, rowAt } from './fixture.ts';
import type { WriteContext } from './write.ts';

export type InvitationScenario =
	'request' | 'cancel' | 'spoof' | 'publish' | 'too-large' | 'outdated' | 'unparsable';

/** Thread positions whose newest message carries an invitation. */
export const INVITATION_INDEX: Record<InvitationScenario, number> = {
	request: 20,
	cancel: 21,
	spoof: 22,
	publish: 23,
	'too-large': 24,
	outdated: 25,
	unparsable: 26
};

export const HOSTILE_DESCRIPTION =
	'Agenda: <img src=x onerror="window.__pwned=\'description\'"> https://docs.example/agenda?id=7, ' +
	'and <script>window.__pwned="script"</script>';

const person = (
	name: string | null,
	email: string,
	partstat: InvitationAttendeeDto['partstat'] = 'needs_action'
): InvitationAttendeeDto => ({ name, email, partstat });

const ORGANIZER = { name: 'Dana Keller', email: 'dana@acme.example', altEmail: null };

const BASE: InvitationEventDto = {
	uid: 'standup-1',
	summary: 'Product sync',
	start: '2026-12-04T08:00:00Z',
	end: '2026-12-04T09:00:00Z',
	allDay: false,
	tzid: 'Europe/Zurich',
	location: 'Room 4.02, Bahnhofstrasse 1',
	description: HOSTILE_DESCRIPTION,
	url: 'https://meet.example/product-sync',
	organizer: ORGANIZER,
	attendees: [
		person('Michel Jung', 'michel@tagmail.test', 'needs_action'),
		person('Ana Ruiz', 'ana@acme.example', 'accepted'),
		person('Ben Okafor', 'ben@acme.example', 'tentative'),
		person('Chi Tran', 'chi@acme.example', 'accepted'),
		person(null, 'dev@acme.example', 'declined'),
		person('Eli Park', 'eli@acme.example', 'accepted'),
		person('Fay Novak', 'fay@acme.example', 'accepted')
	],
	recurrence: {
		frequency: 'weekly',
		interval: 1,
		count: null,
		until: null,
		byDay: [{ ordinal: null, weekday: 'fr' }],
		byMonthDay: [],
		byMonth: [],
		exdates: []
	},
	cancelled: false,
	recurrenceId: null
};

const STATE = {
	stored: null,
	outdated: false,
	target: null,
	scheduling: null,
	autoApply: 'none'
} as const;

function dto(scenario: Exclude<InvitationScenario, 'unparsable'>): InvitationDto {
	switch (scenario) {
		case 'request':
			return {
				method: 'request',
				events: [BASE],
				tooLarge: false,
				truncated: false,
				senderAuth: 'verified',
				...STATE
			};
		case 'cancel':
			return {
				method: 'cancel',
				events: [{ ...BASE, cancelled: true, recurrence: null }],
				tooLarge: false,
				truncated: false,
				senderAuth: 'verified',
				...STATE
			};
		case 'spoof':
			return {
				method: 'request',
				events: [
					{
						...BASE,
						organizer: {
							name: 'Dana Keller',
							email: 'ceo@corp.example',
							altEmail: 'x@evil.example'
						}
					}
				],
				tooLarge: false,
				truncated: false,
				senderAuth: 'address_mismatch',
				...STATE
			};
		case 'publish':
			return {
				method: 'publish',
				events: [
					{
						...BASE,
						uid: 'holiday-1',
						summary: 'Company holiday',
						start: '2026-12-24',
						end: '2026-12-26',
						allDay: true,
						tzid: null,
						location: null,
						description: null,
						url: null,
						attendees: [],
						recurrence: null
					}
				],
				tooLarge: false,
				truncated: true,
				senderAuth: 'unknown',
				...STATE
			};
		case 'outdated':
			return {
				method: 'request',
				events: [
					{
						...BASE,
						start: '2026-12-04T09:00:00',
						end: '2026-12-04T10:00:00',
						tzid: null,
						recurrence: null
					}
				],
				tooLarge: false,
				truncated: false,
				senderAuth: 'verified',
				...STATE,
				outdated: true
			};
		case 'too-large':
			return {
				method: 'none',
				events: [],
				tooLarge: true,
				truncated: false,
				senderAuth: 'unknown',
				...STATE
			};
	}
}

function messageIdOf(scenario: InvitationScenario): number {
	const row = rowAt(INVITATION_INDEX[scenario]);
	return messagesOf(row).at(-1)!.id;
}

const SCENARIO_BY_MESSAGE = new Map(
	(Object.keys(INVITATION_INDEX) as InvitationScenario[]).map((s) => [messageIdOf(s), s])
);

export function invitationThread(scenario: InvitationScenario): number {
	return idAt(INVITATION_INDEX[scenario]);
}

export function hasInvitation(messageId: number): boolean {
	return SCENARIO_BY_MESSAGE.has(messageId);
}

export function calendarApi(
	_req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): boolean {
	const m = /^\/api\/messages\/(\d+)\/invitation$/.exec(url.pathname);
	if (!m) return false;
	const scenario = SCENARIO_BY_MESSAGE.get(Number(m[1]));
	if (scenario === 'unparsable') {
		ctx.error(res, 422, 'unparsable', 'the calendar part is malformed');
	} else if (scenario) ctx.send(res, 200, dto(scenario));
	else ctx.error(res, 404, 'not_found', 'no invitation');
	return true;
}
