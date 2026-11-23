import type { IdentityDto } from '../transport';

/** Marks the signature block in a draft; the sanitizer keeps it and `data-identity`. */
export const SIGNATURE_CLASS = 'tagmail-signature';
/** The usenet signature delimiter ("-- " on a line of its own). */
const DELIMITER = '-- <br>';
const EMPTY_LINE = '<div><br></div>';

const OPEN_BLOCK = new RegExp(`<div\\b[^>]*\\bclass="${SIGNATURE_CLASS}"[^>]*>`, 'i');
const DIV_TAG = /<(\/?)div\b[^>]*>/gi;
const TAG = /<(\/?)([a-z][a-z0-9]*)\b[^>]*>/gi;
const VOID_TAGS = new Set(['br', 'hr', 'img', 'wbr', 'input', 'meta', 'link', 'source']);

type Signed = Pick<IdentityDto, 'id' | 'signature'>;

/** The block a draft carries for `identity`, or `null` when it has no signature. */
export function signatureBlock(identity: Signed): string | null {
	if (!identity.signature) return null;
	return `<div class="${SIGNATURE_CLASS}" data-identity="${identity.id}">${DELIMITER}${identity.signature}</div>`;
}

interface Found {
	start: number;
	end: number;
	inner: string;
}

/** The signature block in `html`, with its offsets and inner HTML. */
export function findSignature(html: string): Found | null {
	const open = OPEN_BLOCK.exec(html);
	if (!open) return null;
	const innerStart = open.index + open[0].length;
	DIV_TAG.lastIndex = innerStart;
	let depth = 1;
	for (let m = DIV_TAG.exec(html); m; m = DIV_TAG.exec(html)) {
		depth += m[1] ? -1 : 1;
		if (depth === 0) {
			return { start: open.index, end: DIV_TAG.lastIndex, inner: html.slice(innerStart, m.index) };
		}
	}
	return null;
}

/** Offsets of the top-level elements of `html`, with their tag names. */
function topLevel(html: string): { start: number; name: string }[] {
	const out: { start: number; name: string }[] = [];
	let depth = 0;
	TAG.lastIndex = 0;
	for (let m = TAG.exec(html); m; m = TAG.exec(html)) {
		const name = m[2].toLowerCase();
		if (VOID_TAGS.has(name)) {
			if (depth === 0) out.push({ start: m.index, name });
			continue;
		}
		if (m[1]) depth = Math.max(0, depth - 1);
		else {
			if (depth === 0) out.push({ start: m.index, name });
			depth++;
		}
	}
	return out;
}

/**
 * Adds `block` where a signature goes: at the end of a new message, and
 * above the quoted original (before its "On … wrote:" line) in a reply or
 * forward.
 */
export function insertSignature(html: string, block: string, quoting: boolean): string {
	if (quoting) {
		const tops = topLevel(html);
		const quote = tops.findIndex((t) => t.name === 'blockquote');
		if (quote !== -1) {
			const at = tops[Math.max(0, quote - 1)].start;
			const head = html.slice(0, at);
			return `${head || EMPTY_LINE}${block}${EMPTY_LINE}${html.slice(at)}`;
		}
	}
	return html ? `${html}${block}` : `${EMPTY_LINE}${block}`;
}

/**
 * The body after switching From to `next`, or `null` to leave it alone.
 * `inserted` is the inner HTML of the block as the editor showed it when
 * it was inserted (`null`: none was); an edited or removed block stays as
 * the writer left it.
 */
export function swapSignature(
	html: string,
	inserted: string | null,
	next: Signed,
	quoting: boolean
): string | null {
	const found = findSignature(html);
	const block = signatureBlock(next);
	if (found) {
		if (inserted === null || found.inner !== inserted) return null;
		return html.slice(0, found.start) + (block ?? '') + html.slice(found.end);
	}
	if (inserted !== null || block === null) return null;
	return insertSignature(html, block, quoting);
}
