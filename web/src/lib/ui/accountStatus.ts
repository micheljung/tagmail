import type { AccountStatusDto } from '@tagmail/api-types';

const PHASES: Record<AccountStatusDto['phase'], string> = {
	pending: 'Starting',
	initial: 'Syncing for the first time',
	catchup: 'Catching up',
	live: 'Up to date',
	stopped: 'Stopped'
};

const DEGRADED: Record<string, string> = {
	no_condstore: 'No CONDSTORE: flag changes sync slower',
	no_qresync: 'No QRESYNC: deletions sync slower',
	no_idle: 'No IDLE: new mail is polled',
	labels_degraded: 'The server drops labels it does not know',
	no_sender_address: 'No address to send from'
};

/** One line on how an account syncs. */
export function phaseText(s: AccountStatusDto | undefined, accountStatus: string): string {
	if (accountStatus === 'disabled') return 'Disabled';
	if (accountStatus === 'deleting') return 'Deleting…';
	if (!s) return 'Not started';
	if (s.lastError && s.phase !== 'live') return s.lastError;
	const text = PHASES[s.phase];
	return s.phase === 'initial' && s.mailboxesTotal > 0
		? `${text} · ${s.mailboxesLive}/${s.mailboxesTotal} folders`
		: text;
}

/** Readable `degraded` codes; per-mailbox ones name the folder. */
export function degradedText(code: string): string {
	const [kind, mailbox] = code.split(/:(.*)/s);
	if (kind === 'no_modseq') return `No MODSEQ in ${mailbox}`;
	if (kind === 'no_keywords') return `${mailbox} can't store labels`;
	return DEGRADED[code] ?? code;
}
