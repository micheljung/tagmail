/** Deterministic mailbox of `THREAD_COUNT` threads for the e2e mock API. */
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

export function messagesOf(row: Row): Message[] {
	const r = rng(row.id * 31);
	const out: Message[] = [];
	for (let k = 0; k < row.messageCount; k++) {
		const from =
			k === row.messageCount - 1 ? row.senders[0] : (row.senders[k % row.senders.length] ?? ME);
		out.push({
			id: row.id * 10 + k,
			from,
			to: [ME],
			cc: k === 0 && r() < 0.3 ? [PEOPLE[(row.id + 3) % PEOPLE.length]] : [],
			date: row.lastDate - (row.messageCount - 1 - k) * Math.round(3600 + r() * 40000),
			subject: k === 0 ? row.subject : `Re: ${row.subject}`,
			snippet: row.snippet,
			unread: k >= row.messageCount - row.unreadCount,
			starred: row.starred && k === row.messageCount - 1,
			hasAttachment: row.hasAttachment && k === 0,
			hasRemoteImages:
				(row.id % 3 === 0 && k === row.messageCount - 1) || positionOf(row.id) === XSS_INDEX
		});
	}
	return out;
}

export function messageById(id: number): { row: Row; message: Message } | undefined {
	const at = positionOf(Math.floor(id / 10));
	if (at === undefined) return undefined;
	const row = rowAt(at);
	const message = messagesOf(row).find((m) => m.id === id);
	return message && { row, message };
}

export function remoteImages(row: Row, m: Message): number {
	if (positionOf(row.id) === XSS_INDEX) return 1;
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
