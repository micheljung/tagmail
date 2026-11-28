import type { IncomingMessage, ServerResponse } from 'node:http';
import type {
	CalApplyDto,
	CaldavDiscoveryDto,
	CaldavSettingsDto,
	CaldavSettingsInputDto,
	CalendarDto,
	CalendarPatchDto,
	CalImportDto,
	CalReplyStateDto,
	CalRsvpDto,
	ConfirmHostDto,
	OpKindDto
} from '../../../packages/api-types/src/index.ts';
import type {
	InvitationAttendeeDto,
	InvitationDto,
	InvitationEventDto,
	StoredEventDto
} from '../../src/lib/calendar/types.ts';
import { ACCOUNT_A, currentIdentities } from './accounts.ts';
import {
	hasCalOp,
	calOp,
	resetCalOps,
	setCalOutcome,
	startCalOp,
	wasDiscarded,
	type CalOutcome
} from './calOps.ts';
import { idAt, messagesOf, rowAt } from './fixture.ts';
import { MAX_EVENTS_SPAN, MAX_TIME, MIN_TIME, eventsIn } from './calendarEvents.ts';
import { mailOpExists, type WriteContext } from './write.ts';

export type InvitationScenario =
	| 'request'
	| 'cancel'
	| 'cancel-manual'
	| 'spoof'
	| 'publish'
	| 'too-large'
	| 'outdated'
	| 'unparsable'
	| 'no-calendar'
	| 'manual'
	| 'reply-not-sent'
	| 'organizer-changed';

/** Thread positions whose newest message carries an invitation. */
export const INVITATION_INDEX: Record<InvitationScenario, number> = {
	request: 20,
	cancel: 21,
	spoof: 22,
	publish: 23,
	'too-large': 24,
	outdated: 25,
	unparsable: 26,
	'no-calendar': 27,
	manual: 28,
	'cancel-manual': 29,
	'reply-not-sent': 30,
	'organizer-changed': 31
};

const OP_ID_PATTERN = /^[A-Za-z0-9._-]{1,64}$/;
const RSVP_PARTSTATS: ReadonlySet<string> = new Set(['accepted', 'tentative', 'declined']);

const TARGET = { accountId: ACCOUNT_A, calendarHref: '/dav/personal/' };

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
	target: TARGET,
	scheduling: null,
	autoApply: 'none',
	organizerChanged: false
} as const;

function dto(scenario: Exclude<InvitationScenario, 'unparsable'>): InvitationDto {
	switch (scenario) {
		case 'no-calendar':
			return { ...dto('request'), target: null };
		case 'cancel-manual':
			return {
				...dto('cancel'),
				stored: {
					accountId: ACCOUNT_A,
					calendarHref: TARGET.calendarHref,
					partstat: 'accepted',
					sequence: 1,
					status: 'confirmed',
					replyState: null,
					revertPartstat: null
				},
				autoApply: 'manual'
			};
		case 'manual':
			return {
				...dto('request'),
				stored: {
					accountId: 1,
					calendarHref: TARGET.calendarHref,
					partstat: 'accepted',
					sequence: 1,
					status: 'confirmed',
					replyState: null,
					revertPartstat: null
				},
				autoApply: 'manual'
			};
		case 'reply-not-sent':
			return {
				...dto('manual'),
				stored: {
					...dto('manual').stored!,
					partstat: 'tentative',
					replyState: 'cancelled',
					revertPartstat: 'accepted'
				},
				autoApply: 'none'
			};
		case 'organizer-changed':
			return {
				...dto('manual'),
				events: [{ ...BASE, organizer: { ...ORGANIZER, email: 'lee@acme.example' } }],
				organizerChanged: true
			};
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

export const DAV_HOST = 'dav.provider.example';
export const AUTHSERV_SUGGESTION = 'mx.provider.example';

type CaldavScenario = 'unconfigured' | 'configured';

let caldav = new Map<number, CaldavSettingsDto>();
let calendars: CalendarDto[] = [];
let confirms: ConfirmHostDto[] = [];
let puts: { accountId: number; body: CaldavSettingsInputDto }[] = [];
let patches: CalendarPatchDto[] = [];
let seenEventQueries: string[] = [];

function blankSettings(): CaldavSettingsDto {
	return {
		url: null,
		username: null,
		hasPassword: false,
		allowInsecure: false,
		scheduling: 'auto',
		schedulingDetected: null,
		fallbackAccountId: null,
		defaultCalendarHref: null,
		authservId: null,
		authservIdSuggestion: AUTHSERV_SUGGESTION,
		confirmedHost: null,
		status: 'unconfigured',
		lastError: null
	};
}

const calendar = (
	href: string,
	displayName: string,
	color: string,
	extra: Partial<CalendarDto> = {}
): CalendarDto => ({
	accountId: ACCOUNT_A,
	href,
	displayName,
	color,
	visible: true,
	sort: null,
	writable: true,
	isDefault: false,
	...extra
});

function foundCalendars(): CalendarDto[] {
	return [
		calendar('/dav/personal/', 'Personal', '#2350c8', { isDefault: true }),
		calendar('/dav/work/', 'Work', '#1d7a4f'),
		calendar('/dav/holidays/', 'Holidays', '#c2362b', { writable: false })
	];
}

export function resetCaldav(scenario: CaldavScenario = 'unconfigured'): void {
	caldav = new Map([[ACCOUNT_A, blankSettings()]]);
	calendars = [];
	puts = [];
	patches = [];
	seenEventQueries = [];
	confirms = [];
	if (scenario === 'configured') {
		caldav.set(ACCOUNT_A, {
			...blankSettings(),
			defaultCalendarHref: '/dav/personal/',
			schedulingDetected: 'server',
			confirmedHost: DAV_HOST,
			status: 'ok'
		});
		calendars = foundCalendars();
	}
}
resetCaldav();

function settingsOf(accountId: number): CaldavSettingsDto {
	let s = caldav.get(accountId);
	if (!s) {
		s = blankSettings();
		caldav.set(accountId, s);
	}
	return s;
}

function discovery(accountId: number): CaldavDiscoveryDto {
	const s = settingsOf(accountId);
	return {
		status: s.status,
		host: s.confirmedHost ?? DAV_HOST,
		url: s.url,
		schedulingDetected: s.schedulingDetected,
		calendars: calendars.filter((c) => c.accountId === accountId),
		error: null
	};
}

function invalid(input: CaldavSettingsInputDto, accountId: number): [string, string] | null {
	const s = settingsOf(accountId);
	const url = input.url?.trim() || null;
	const username = input.username?.trim() || null;
	if (url) {
		if (/^https?:\/\/[^/]*@/.test(url)) return ['url', 'the URL must not contain a login'];
		if (url.startsWith('http://') && !input.allowInsecure) {
			return ['url', 'http needs the unencrypted connection to be allowed'];
		}
	}
	if (!username && input.password) return ['username', 'a password needs a username'];
	const sameLogin = s.hasPassword && s.username === username && s.url === url;
	if (username && !input.password && !sameLogin) {
		return ['password', 'enter the password for a new username or server'];
	}
	if (input.fallbackAccountId === accountId) {
		return ['fallbackAccountId', 'an account cannot fall back to itself'];
	}
	return null;
}

function applyInput(accountId: number, input: CaldavSettingsInputDto): void {
	const s = settingsOf(accountId);
	const url = input.url ?? null;
	const username = input.username ?? null;
	const keepsPassword = username === s.username && url === s.url;
	caldav.set(accountId, {
		...s,
		url,
		username,
		hasPassword: username !== null && (input.password ? true : keepsPassword && s.hasPassword),
		allowInsecure: input.allowInsecure,
		scheduling: input.scheduling,
		fallbackAccountId: input.fallbackAccountId ?? null,
		defaultCalendarHref: input.defaultCalendarHref ?? null,
		authservId: 'authservId' in input ? (input.authservId ?? null) : s.authservId
	});
}

export function caldavState() {
	return {
		settings: Object.fromEntries(caldav),
		calendars,
		puts,
		patches,
		confirms,
		eventQueries: seenEventQueries
	};
}

const changed = new Map<number, Partial<InvitationDto>>();

export function resetCalendar(): void {
	changed.clear();
	resetCalOps();
}

function invitationOf(messageId: number): InvitationDto | undefined {
	const scenario = SCENARIO_BY_MESSAGE.get(messageId);
	if (!scenario || scenario === 'unparsable') return undefined;
	return { ...dto(scenario), ...changed.get(messageId) };
}

function store(messageId: number, patch: Partial<InvitationDto>): void {
	changed.set(messageId, { ...changed.get(messageId), ...patch });
}

function stored(
	href: string,
	partstat: StoredEventDto['partstat'],
	previous: StoredEventDto | null,
	status: StoredEventDto['status'] = 'confirmed',
	replyState: StoredEventDto['replyState'] = previous?.replyState ?? null,
	revertPartstat: StoredEventDto['revertPartstat'] = previous?.revertPartstat ?? null
): StoredEventDto {
	return {
		accountId: 1,
		calendarHref: href,
		partstat,
		sequence: (previous?.sequence ?? 0) + 1,
		status,
		replyState,
		revertPartstat
	};
}

const BODY_LIMIT_NOTE = 'calendar request body must be JSON';

export async function calendarApi(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	const method = req.method ?? 'GET';
	const json = async <T>() => JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as T;

	const inv = /^\/api\/messages\/(\d+)\/invitation$/.exec(url.pathname);
	if (inv) {
		const scenario = SCENARIO_BY_MESSAGE.get(Number(inv[1]));
		if (scenario === 'unparsable') {
			ctx.error(res, 422, 'unparsable', 'the calendar part is malformed');
		} else if (scenario) ctx.send(res, 200, invitationOf(Number(inv[1])));
		else ctx.error(res, 404, 'not_found', 'no invitation');
		return true;
	}

	if (url.pathname === '/api/calendars') {
		if (method === 'GET') ctx.send(res, 200, { calendars });
		else if (method === 'PATCH') {
			const patch = await json<CalendarPatchDto>();
			patches.push(patch);
			const target = calendars.find(
				(c) => c.accountId === patch.accountId && c.href === patch.href
			);
			if (!target) {
				ctx.error(res, 404, 'not_found', 'no such calendar');
				return true;
			}
			if (patch.color !== undefined) target.color = patch.color;
			if (patch.visible !== undefined) target.visible = patch.visible;
			if (patch.default) {
				for (const c of calendars) {
					if (c.accountId === patch.accountId) c.isDefault = c.href === patch.href;
				}
				settingsOf(patch.accountId).defaultCalendarHref = patch.href;
			}
			ctx.send(res, 200, target);
		} else return false;
		return true;
	}

	if (url.pathname === '/api/calendar/events' && method === 'GET') {
		const refuse = (field: string, message: string) =>
			ctx.send(res, 422, { error: 'invalid', field, message });
		const bound = (name: string) => {
			const v = url.searchParams.get(name);
			const n = v !== null && /^[+-]?\d+$/.test(v) ? BigInt(v) : null;
			return n !== null && n >= -(2n ** 63n) && n < 2n ** 63n ? n : null;
		};
		const [from, to] = [bound('from'), bound('to')];
		if (from === null || to === null) {
			ctx.error(res, 400, 'bad_request', 'from and to must be Unix seconds');
			return true;
		}
		const representable = (t: bigint) => t >= BigInt(MIN_TIME) && t <= BigInt(MAX_TIME);
		if (!representable(from)) {
			refuse('from', 'not a representable time');
			return true;
		}
		if (!representable(to) || to <= from || to - from > BigInt(MAX_EVENTS_SPAN)) {
			refuse('to', 'after from and at most 62 days later');
			return true;
		}
		const wanted = [];
		for (const value of url.searchParams.getAll('calendars')) {
			const at = value.indexOf(':');
			const accountId = Number(value.slice(0, at));
			if (at < 0 || !/^[+-]?\d+$/.test(value.slice(0, at))) {
				refuse('calendars', 'each is <accountId>:<href>');
				return true;
			}
			wanted.push({ accountId, calendarHref: value.slice(at + 1) });
		}
		seenEventQueries.push(url.search);
		ctx.send(res, 200, { events: eventsIn(calendars, Number(from), Number(to), wanted) });
		return true;
	}

	if (await calendarOpsApi(req, res, url, ctx)) return true;

	const dav = /^\/api\/accounts\/(\d+)\/caldav(?:\/(discover|confirm-host))?$/.exec(url.pathname);
	if (!dav) return false;
	const accountId = Number(dav[1]);
	const action = dav[2];
	if (!action && method === 'GET') ctx.send(res, 200, settingsOf(accountId));
	else if (!action && method === 'PUT') {
		const input = await json<CaldavSettingsInputDto>();
		const problem = invalid(input, accountId);
		if (problem) {
			ctx.send(res, 422, { error: 'invalid', message: problem[1], field: problem[0] });
			return true;
		}
		puts.push({ accountId, body: input });
		applyInput(accountId, input);
		ctx.send(res, 200, settingsOf(accountId));
	} else if (action === 'discover' && method === 'POST') {
		const s = settingsOf(accountId);
		if (!s.confirmedHost) caldav.set(accountId, { ...s, status: 'needs_confirmation' });
		else caldav.set(accountId, { ...s, status: 'ok' });
		ctx.send(res, 200, discovery(accountId));
	} else if (action === 'confirm-host' && method === 'POST') {
		let host: unknown;
		try {
			host = (await json<Partial<ConfirmHostDto>>()).host;
		} catch {
			host = undefined;
		}
		if (typeof host !== 'string' || host === '') {
			ctx.error(res, 400, 'bad_request', 'a host is required');
			return true;
		}
		if (host !== DAV_HOST) {
			ctx.error(res, 422, 'invalid', 'that is not the host discovery asked about');
			return true;
		}
		confirms.push({ host });
		const s = settingsOf(accountId);
		caldav.set(accountId, {
			...s,
			confirmedHost: DAV_HOST,
			status: 'ok',
			schedulingDetected: 'server',
			defaultCalendarHref: '/dav/personal/'
		});
		calendars = [...calendars.filter((c) => c.accountId !== accountId), ...foundCalendars()];
		ctx.send(res, 200, discovery(accountId));
	} else return false;
	return true;
}

async function calendarOpsApi(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	const refuse = (field: string, message: string) =>
		ctx.send(res, 422, { error: 'invalid', field, message });
	const method = req.method ?? 'GET';
	const route = /^\/api\/calendar\/(rsvp|import|apply)$/.exec(url.pathname);
	if (!route || method !== 'POST') return false;
	let body: Partial<CalRsvpDto & CalImportDto & CalApplyDto>;
	try {
		body = JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as typeof body;
	} catch {
		ctx.error(res, 400, 'bad_request', BODY_LIMIT_NOTE);
		return true;
	}

	if (typeof body.opId !== 'string' || !OP_ID_PATTERN.test(body.opId)) {
		refuse('opId', "1 to 64 characters of letters, digits, '-', '_' or '.'");
		return true;
	}
	const messageId = Number(body.messageId);
	const invitation = Number.isInteger(messageId) ? invitationOf(messageId) : undefined;
	if (!invitation) {
		ctx.error(res, 404, 'not_found', 'no invitation');
		return true;
	}
	const known = calOp(body.opId);
	if (known) {
		ctx.send(res, 200, known);
		return true;
	}
	if (mailOpExists(body.opId) || hasCalOp(body.opId) || wasDiscarded(body.opId)) {
		ctx.error(res, 409, 'conflict', 'the op id is already used by another op');
		return true;
	}

	const event = invitation.events[0];
	const target = invitation.target;
	let kind: OpKindDto = 'cal.apply';
	let calendarHref: string | null = target?.calendarHref ?? null;
	let reply: 'queued' | null = null;
	let onDone: (reply: CalReplyStateDto | null) => void;

	if (route[1] === 'rsvp') {
		kind = 'cal.rsvp';
		const sendReply = body.sendReply !== false;
		if (
			!RSVP_PARTSTATS.has(String(body.partstat)) &&
			(sendReply || body.partstat !== 'needs_action')
		) {
			refuse('partstat', 'accepted, tentative or declined');
			return true;
		}
		if (!sendReply && !target) {
			refuse('messageId', 'no_calendar');
			return true;
		}
		const mine = new Set(currentIdentities().map((i) => i.address.toLowerCase()));
		const attends = event.attendees.some((a) => mine.has(a.email.toLowerCase()));
		const named = currentIdentities().find((i) => i.id === body.identityId);
		const attending =
			body.identityId === undefined
				? attends
				: !!named && event.attendees.some((a) => a.email.toLowerCase() === named.address);
		if (!attending) {
			refuse('identityId', 'none of your identities is an attendee of this event');
			return true;
		}
		if (target && invitation.organizerChanged && body.allowOrganizerChange !== true) {
			refuse('allowOrganizerChange', 'the update names another organizer than the calendar event');
			return true;
		}
		reply = sendReply ? 'queued' : null;
		const partstat = body.partstat as NonNullable<StoredEventDto['partstat']>;
		onDone = (replyState) => {
			const before = invitation.stored?.partstat ?? 'needs_action';
			if (calendarHref)
				store(messageId, {
					stored: stored(
						calendarHref,
						partstat,
						invitation.stored,
						'confirmed',
						replyState,
						replyState === 'cancelled' ? before : null
					),
					organizerChanged: false
				});
		};
	} else if (route[1] === 'import') {
		kind = 'cal.import';
		if ((body.accountId === undefined) !== (body.calendarHref === undefined)) {
			refuse('calendarHref', 'accountId and calendarHref go together');
			return true;
		}
		if (body.calendarHref !== undefined) {
			const chosen = calendars.find(
				(c) => c.accountId === body.accountId && c.href === body.calendarHref
			);
			if (!chosen?.writable) {
				refuse('calendarHref', 'unknown_calendar');
				return true;
			}
			calendarHref = chosen.href;
		}
		if (!calendarHref) {
			refuse('calendarHref', 'no_calendar');
			return true;
		}
		const href = calendarHref;
		onDone = () => store(messageId, { stored: stored(href, null, invitation.stored) });
	} else {
		if (!target) {
			refuse('messageId', 'no_calendar');
			return true;
		}
		if (invitation.organizerChanged && body.allowOrganizerChange !== true) {
			refuse('allowOrganizerChange', 'the update names another organizer than the calendar event');
			return true;
		}
		onDone = () =>
			store(messageId, {
				stored: stored(
					target.calendarHref,
					invitation.stored?.partstat ?? null,
					invitation.stored,
					invitation.method === 'cancel' ? 'cancelled' : 'confirmed'
				),
				autoApply: 'applied',
				outdated: false,
				organizerChanged: false
			});
	}

	const op = startCalOp(ctx, {
		opId: body.opId,
		kind,
		accountId: 1,
		uid: event.uid,
		targetAccountId: body.accountId ?? target?.accountId ?? 1,
		calendarHref,
		reply,
		onDone
	});
	ctx.send(res, 202, op);
	return true;
}

/** Test hooks: `POST /__mock/caldav {scenario}` resets, `GET` shows what the app sent. */
export async function calendarMock(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	if (url.pathname !== '/__mock/caldav') return false;
	if (req.method === 'POST') {
		const { scenario } = JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as {
			scenario?: CaldavScenario;
		};
		resetCaldav(scenario === 'configured' ? 'configured' : 'unconfigured');
		ctx.send(res, 204, '');
	} else ctx.send(res, 200, caldavState());
	return true;
}

export async function calendarOutcomeMock(
	req: IncomingMessage,
	res: ServerResponse,
	url: URL,
	ctx: WriteContext
): Promise<boolean> {
	if (url.pathname !== '/__mock/calendar-outcome' || req.method !== 'POST') return false;
	const { outcome } = JSON.parse((await ctx.readBody(req)).toString('utf8') || '{}') as {
		outcome?: CalOutcome;
	};
	setCalOutcome(outcome ?? 'ok');
	ctx.send(res, 204, '');
	return true;
}
