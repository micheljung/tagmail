import type { AddressDto } from '@tagmail/api-types';

export interface Recipient extends AddressDto {
	valid: boolean;
}

/** Practical check, not RFC 5322: one @, a dotted domain, no spaces or brackets. */
const EMAIL = /^[^\s@<>()[\]",;:]+@[^\s@<>()[\]",;:]+\.[^\s@<>()[\]",;:.]{2,}$/;

export function isValidEmail(email: string): boolean {
	return EMAIL.test(email);
}

/** Splits on commas, semicolons and newlines outside quotes and angle brackets. */
function tokens(text: string): string[] {
	const out: string[] = [];
	let cur = '';
	let quoted = false;
	let angle = false;
	for (const ch of text) {
		if (ch === '"') quoted = !quoted;
		else if (ch === '<' && !quoted) angle = true;
		else if (ch === '>' && !quoted) angle = false;
		if (!quoted && !angle && (ch === ',' || ch === ';' || ch === '\n')) {
			out.push(cur);
			cur = '';
		} else cur += ch;
	}
	out.push(cur);
	return out.map((t) => t.trim()).filter(Boolean);
}

export function parseAddress(token: string): Recipient {
	const m = /^(.*?)<([^>]*)>\s*$/.exec(token);
	if (m) {
		const name = m[1]
			.trim()
			.replace(/^"(.*)"$/, '$1')
			.trim();
		const email = m[2].trim();
		return { name: name || null, email, valid: isValidEmail(email) };
	}
	const email = token.trim().replace(/^mailto:/i, '');
	return { name: null, email, valid: isValidEmail(email) };
}

/** Everything typed or pasted into a recipient field, one entry per address. */
export function parseAddresses(text: string): Recipient[] {
	return tokens(text).map(parseAddress);
}

export function toRecipient(a: AddressDto): Recipient {
	return { name: a.name, email: a.email, valid: isValidEmail(a.email) };
}

export function formatRecipient(r: AddressDto): string {
	return r.name ? `${r.name} <${r.email}>` : r.email;
}
