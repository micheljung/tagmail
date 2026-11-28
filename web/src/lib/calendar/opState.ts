import type { CalOpInfoDto, OpKindDto } from '../transport';

const DESCRIPTIONS: Partial<Record<OpKindDto, (uid: string) => string>> = {
	'cal.rsvp': (uid) => `Reply to invitation ${uid}`,
	'cal.import': (uid) => `Add event ${uid} to calendar`,
	'cal.apply': (uid) => `Update event ${uid} in calendar`,
	'cal.put': (uid) => `Save event ${uid}`,
	'cal.delete': (uid) => `Delete event ${uid}`,
	'cal.move': (uid) => `Move event ${uid}`
};

export const isCalendarKind = (kind: OpKindDto): boolean => kind.startsWith('cal.');

export function describeCalendarOp(kind: OpKindDto, calendar: CalOpInfoDto): string {
	return DESCRIPTIONS[kind]?.(calendar.uid) ?? `Change event ${calendar.uid}`;
}

export interface CalendarNote {
	text: string;
	retryLabel: string;
}

/** What a failed or half-done calendar op means to the person; `null` when nothing special. */
export function calendarNote(status: string, calendar: CalOpInfoDto): CalendarNote | null {
	const invites = /#(request|cancel)$/.test(calendar.replyOpId ?? '');
	if (invites && calendar.reply === 'cancelled') {
		return { text: 'Guests were not told about this change', retryLabel: 'Retry' };
	}
	if (status === 'failed' && invites && calendar.conflict !== 'server_won') {
		return { text: "Your edit wasn't saved and guests were not told", retryLabel: 'Retry' };
	}
	if (calendar.conflict === 'server_won') {
		return {
			text: `Calendar changed on the server — your edit wasn't saved${invites ? ' and guests were not told' : ''}`,
			retryLabel: 'Re-apply'
		};
	}
	if (status === 'failed' && calendar.reply === 'sent') {
		return { text: 'Reply sent, not saved to calendar', retryLabel: 'Retry' };
	}
	if (calendar.reply === 'cancelled') return { text: 'Reply not sent', retryLabel: 'Retry' };
	return null;
}
