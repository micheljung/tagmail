/** Deterministic mailbox of `THREAD_COUNT` threads for the e2e mock API. */
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

export const THREAD_COUNT = 100_000;
export const NOW = Date.UTC(2026, 10, 20, 16, 42) / 1000;
export const XSS_INDEX = 7;
export const PASSWORD = 'correct horse battery staple';
export const SETUP_TOKEN = 'TAGM-SETUP-2026';

export interface Address {
	name: string | null;
	email: string;
}

export interface Row {
	id: number;
	accountId: number;
	subject: string;
	snippet: string;
	lastDate: number;
	messageCount: number;
	unreadCount: number;
	starred: boolean;
	hasAttachment: boolean;
	senders: Address[];
	labels: string[];
	/** Mailboxes whose list shows the thread. */
	mailboxIds: number[];
}

export const LABELS = [
	{ key: 'clients', name: 'Clients' },
	{ key: 'finance', name: 'Finance' },
	{ key: 'family', name: 'Family' },
	{ key: 'receipts', name: 'Receipts' },
	{ key: 'travel', name: 'Travel' },
	{ key: 'work/hiring', name: 'Work/Hiring' },
	{ key: 'newsletters', name: 'Newsletters' },
	{ key: 'side project', name: 'Side project' }
];

/**
 * Label of the rendering corpus rows; kept out of `LABELS` so specs that pick
 * or look up fixture labels never meet it.
 */
export const CORPUS_LABEL = { key: 'corpus', name: 'Corpus' };

export interface CorpusEmail {
	name: string;
	tags: string[];
	/** Text without an author `font-size`, for the font-size checks. */
	unstyledTextSelector?: string;
}

export const CORPUS_DIR = join(import.meta.dirname, '../corpus');
export const CORPUS = JSON.parse(
	readFileSync(join(CORPUS_DIR, 'manifest.json'), 'utf8')
) as CorpusEmail[];
/** The corpus takes the oldest list positions, so the thread count stays the same. */
const CORPUS_START = THREAD_COUNT - CORPUS.length;

function corpusAt(i: number): CorpusEmail | undefined {
	return i >= CORPUS_START && i < THREAD_COUNT ? CORPUS[i - CORPUS_START] : undefined;
}

/** The corpus email a thread shows, if any. */
export function corpusOf(threadId: number): CorpusEmail | undefined {
	const at = positionOf(threadId);
	return at === undefined ? undefined : corpusAt(at);
}

/** Thread id of the corpus email `name`. */
export function corpusThreadId(name: string): number {
	const k = CORPUS.findIndex((c) => c.name === name);
	if (k < 0) throw new Error(`no corpus email ${name}`);
	return idAt(CORPUS_START + k);
}

const CORPUS_SENDER: Address = { name: 'Sample Sender', email: 'sender@example.com' };

export const MAILBOXES = [
	{ id: 1, accountId: 1, name: 'INBOX', delimiter: '/', specialUse: null },
	{ id: 2, accountId: 1, name: 'Sent', delimiter: '/', specialUse: 'sent' },
	{ id: 3, accountId: 1, name: 'Archive', delimiter: '/', specialUse: 'archive' },
	{ id: 4, accountId: 1, name: 'Trash', delimiter: '/', specialUse: 'trash' },
	{ id: 5, accountId: 1, name: 'Junk', delimiter: '/', specialUse: 'junk' },
	{ id: 6, accountId: 1, name: 'Projects', delimiter: '/', specialUse: null }
];

const PEOPLE: Address[] = [
	{ name: 'Maya Lindqvist', email: 'maya@lindqvist.studio' },
	{ name: 'Jonas Albrecht', email: 'jonas.albrecht@fieldnotes.io' },
	{ name: 'Priya Raman', email: 'priya@ramanlabs.dev' },
	{ name: 'Tomás Herrera', email: 'tomas@herrera.coop' },
	{ name: 'Elif Demir', email: 'elif.demir@kayak-club.org' },
	{ name: 'GitHub', email: 'noreply@github.com' },
	{ name: 'Swiss Rail', email: 'tickets@rail.example' },
	{ name: 'Ana Sousa', email: 'ana@sousa.family' },
	{ name: 'Northwind Billing', email: 'billing@northwind.example' },
	{ name: 'Kenji Watanabe', email: 'kenji@watanabe.jp' },
	{ name: null, email: 'office@coworking-zurich.ch' },
	{ name: 'Lea Meier', email: 'lea.meier@meier-partner.ch' },
	{ name: 'The Weekly Diff', email: 'hello@weeklydiff.news' },
	{ name: 'Oskar Nyberg', email: 'oskar@nyberg.se' }
];

const SUBJECTS = [
	'Draft contract for the spring engagement',
	'Re: Kayak weekend — who brings the van?',
	'Your invoice INV-2026-0412 is ready',
	'Interview loop for the backend role',
	'Photos from Lisbon',
	'Q4 numbers and the board deck',
	'[tagmail] CI: e2e job is green again',
	'Train tickets Zürich → Milano',
	'Coffee next week?',
	'Re: Re: Apartment handover checklist',
	'Your receipt from Northwind',
	'Notes from today’s design review',
	'Grandma’s 90th — planning thread',
	'This week: zero-copy parsers and SQLite tricks',
	'Proposal: move standup to 9:45',
	'Feedback on the onboarding flow'
];

const SNIPPETS = [
	'Attached is the revised version with your comments folded in. The only open point is the payment schedule',
	'I can drive if someone else handles the paddles. Weather looks good for Saturday morning, maybe windy after',
	'Thanks for your business. The amount due is CHF 1,240.00 and will be charged on the first of next month',
	'Here is the panel and the schedule. Could you take the system design slot on Thursday afternoon?',
	'Finally sorted them — the ones from the tram are my favorite. Shared album link below, no rush',
	'Revenue landed slightly above plan; churn is flat. I moved the hiring slide to the appendix',
	'All 214 tests passed on Chromium and WebKit. Jump-to-row timing is well under the gate',
	'Your seats are reserved in coach 7. Bring the QR code on your phone, printing is not required',
	'I’m around Tuesday or Thursday after 3. There’s a new place near the lake that does great cortados',
	'Keys go in the mailbox; meter readings are in the photo. Let me know if anything is missing',
	'We received your payment. This message confirms your order and is your receipt for your records',
	'Main takeaways: tighten the list density, keep labels as tags, and ship dark mode with the first cut',
	'Aunt Rosa booked the hall. We still need someone to coordinate the cake and the slideshow',
	'Plus: why your FTS index is bigger than you think, and a tiny trick for varint decoding',
	'Half the team is on the train at 9:30, so a later slot would mean fewer people joining from phones',
	'The second step asks for too much at once. What if we split account and server settings?'
];

function rng(seed: number): () => number {
	let a = seed >>> 0;
	return () => {
		a = (a + 0x6d2b79f5) >>> 0;
		let t = a;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

/** Thread id at list position `i`: a permutation, so ids are not monotonic. */
export function idAt(i: number): number {
	return 1 + ((i * 7919) % THREAD_COUNT);
}

const positions = new Map<number, number>();
for (let i = 0; i < THREAD_COUNT; i++) positions.set(idAt(i), i);

export function positionOf(id: number): number | undefined {
	return positions.get(id);
}

export function rowAt(i: number): Row {
	const r = rng(i + 1);
	const pick = <T>(list: readonly T[]) => list[Math.floor(r() * list.length)];
	const topic = Math.floor(r() * SUBJECTS.length);
	const messageCount = r() < 0.6 ? 1 : 2 + Math.floor(r() * 4);
	const senders: Address[] = [];
	for (let k = 0; k < Math.min(messageCount, 3); k++) {
		const p = pick(PEOPLE);
		if (!senders.includes(p)) senders.push(p);
	}
	const labels = LABELS.filter(() => r() < 0.08).map((l) => l.key);
	const unread = r() < (i < 40 ? 0.45 : 0.12);
	const xss = i === XSS_INDEX;
	const corpus = corpusAt(i);
	if (corpus) {
		return {
			id: idAt(i),
			accountId: 1,
			subject: `Corpus: ${corpus.name}`,
			snippet: `Rendering corpus email (${corpus.tags.join(', ')})`,
			lastDate: Math.round(NOW - i * 1260),
			messageCount: 1,
			unreadCount: 0,
			starred: false,
			hasAttachment: false,
			senders: [CORPUS_SENDER],
			labels: [CORPUS_LABEL.key],
			mailboxIds: [1]
		};
	}
	return {
		id: idAt(i),
		accountId: 1,
		subject: xss ? 'Security corpus: hostile HTML sample' : SUBJECTS[topic],
		snippet: xss ? 'This message tries every trick to run script in your browser' : SNIPPETS[topic],
		lastDate: Math.round(NOW - i * 1260 - r() * 900),
		messageCount,
		unreadCount: unread ? 1 + Math.floor(r() * Math.min(messageCount, 2)) : 0,
		starred: r() < 0.06,
		hasAttachment: r() < 0.12,
		senders,
		labels: labels.sort(),
		mailboxIds: [1]
	};
}

export function isSent(i: number): boolean {
	return rng(i + 7_777_777)() < 0.1;
}

export interface Message {
	id: number;
	from: Address | null;
	to: Address[];
	cc: Address[];
	date: number;
	subject: string | null;
	snippet: string;
	unread: boolean;
	starred: boolean;
	hasAttachment: boolean;
	hasRemoteImages: boolean | null;
}

export interface Part {
	id: number;
	contentType: string;
	name: string | null;
	size: number;
	contentId: string | null;
	inline: boolean;
	url: string;
}

export const ME: Address = { name: 'Michel Jung', email: 'michel@tagmail.test' };

/** Account 2 of the `multi` scenario: a small second mailbox with an alias. */
export const ACCOUNT_B = 2;
export const B_THREAD_COUNT = 50;
const B_ID_BASE = 200_000;
/** Seconds between account 2's threads, so they interleave with account 1's newest rows. */
const B_SPACING = 2520;
export const B_ME: Address = { name: 'Michel Jung', email: 'michel@bergmail.example' };
export const B_ALIAS: Address = { name: 'Berg Team', email: 'team@bergmail.example' };

export const B_MAILBOXES = [
	{ id: 11, accountId: ACCOUNT_B, name: 'INBOX', delimiter: '/', specialUse: null },
	{ id: 12, accountId: ACCOUNT_B, name: 'Sent', delimiter: '/', specialUse: 'sent' },
	{ id: 13, accountId: ACCOUNT_B, name: 'Archive', delimiter: '/', specialUse: 'archive' },
	{ id: 14, accountId: ACCOUNT_B, name: 'Trash', delimiter: '/', specialUse: 'trash' },
	{ id: 15, accountId: ACCOUNT_B, name: 'Junk', delimiter: '/', specialUse: 'junk' },
	{ id: 16, accountId: ACCOUNT_B, name: 'Suppliers', delimiter: '/', specialUse: null }
];

const B_SUBJECTS = [
	'Berg cabin booking for the long weekend',
	'Supplier quote: timber for the workshop',
	'Club newsletter — winter schedule',
	'Re: Shared drive cleanup'
];

export function bIdAt(j: number): number {
	return B_ID_BASE + 1 + j;
}

export function bPositionOf(id: number): number | undefined {
	const j = id - B_ID_BASE - 1;
	return j >= 0 && j < B_THREAD_COUNT ? j : undefined;
}

export function bRowAt(j: number): Row {
	const r = rng(B_ID_BASE + j);
	const topic = j % B_SUBJECTS.length;
	return {
		id: bIdAt(j),
		accountId: ACCOUNT_B,
		subject: B_SUBJECTS[topic],
		snippet: SNIPPETS[(topic + 3) % SNIPPETS.length],
		lastDate: Math.round(NOW - 300 - j * B_SPACING),
		messageCount: 1,
		unreadCount: r() < 0.4 ? 1 : 0,
		starred: false,
		hasAttachment: false,
		senders: [PEOPLE[(j * 5) % PEOPLE.length]],
		labels: [],
		mailboxIds: [B_MAILBOXES[0].id]
	};
}

/** Even account 2 threads are addressed to the alias. */
function recipientOf(row: Row): Address {
	if (row.accountId !== ACCOUNT_B) return ME;
	return bPositionOf(row.id)! % 2 === 0 ? B_ALIAS : B_ME;
}

export function messagesOf(row: Row): Message[] {
	const r = rng(row.id * 31);
	const out: Message[] = [];
	for (let k = 0; k < row.messageCount; k++) {
		const from =
			k === row.messageCount - 1
				? row.senders[0]
				: (row.senders[k % row.senders.length] ?? recipientOf(row));
		out.push({
			id: row.id * 10 + k,
			from,
			to: [recipientOf(row)],
			cc: k === 0 && r() < 0.3 ? [PEOPLE[(row.id + 3) % PEOPLE.length]] : [],
			date: row.lastDate - (row.messageCount - 1 - k) * Math.round(3600 + r() * 40000),
			subject: k === 0 ? row.subject : `Re: ${row.subject}`,
			snippet: row.snippet,
			unread: k >= row.messageCount - row.unreadCount,
			starred: row.starred && k === row.messageCount - 1,
			hasAttachment: row.hasAttachment && k === 0,
			hasRemoteImages:
				(row.id % 3 === 0 && k === row.messageCount - 1 && !corpusOf(row.id)) ||
				positionOf(row.id) === XSS_INDEX
		});
	}
	return out;
}

export function messageById(id: number): { row: Row; message: Message } | undefined {
	const threadId = Math.floor(id / 10);
	const at = positionOf(threadId);
	const bAt = bPositionOf(threadId);
	if (at === undefined && bAt === undefined) return undefined;
	const row = at === undefined ? bRowAt(bAt!) : rowAt(at);
	const message = messagesOf(row).find((m) => m.id === id);
	return message && { row, message };
}

export function remoteImages(row: Row, m: Message): number {
	if (positionOf(row.id) === XSS_INDEX) return 1;
	if (corpusOf(row.id)) return 0;
	return row.id % 3 === 0 && m.id === row.id * 10 + row.messageCount - 1 ? 1 : 0;
}

export function partsOf(m: Message): Part[] {
	if (!m.hasAttachment) return [];
	const exp = Math.floor(Date.now() / 1000) + 600;
	return [
		{
			id: 2,
			contentType: 'application/pdf',
			name: 'Statement-November.pdf',
			size: 248_331,
			contentId: null,
			inline: false,
			url: `/api/parts/m${m.id}p2?exp=${exp}&sig=mock`
		}
	];
}
