import type { Config, DOMPurify } from 'dompurify';

/**
 * What may enter the editor from paste, quotes and saved drafts. Style
 * attributes would trip the app CSP (no inline styles), and remote images
 * would load straight from the sender's server while composing.
 */
export const COMPOSE_PURIFY: Config = {
	FORBID_TAGS: [
		'style',
		'img',
		'picture',
		'video',
		'audio',
		'iframe',
		'object',
		'embed',
		'form',
		'input',
		'button',
		'select',
		'textarea',
		'svg',
		'math',
		'link',
		'meta',
		'base'
	],
	FORBID_ATTR: ['style', 'class', 'id', 'srcset', 'background'],
	ALLOW_DATA_ATTR: false,
	ALLOWED_URI_REGEXP: /^(?:https?|mailto|tel):/i
};

const STYLE_ELEMENT = /<style\b[\s\S]*?(?:<\/style\s*>|$)/gi;
const STYLE_ATTRIBUTE = /\sstyle\s*=\s*(?:"[^"]*"|'[^']*'|[^\s>]+)/gi;

/**
 * Chromium reports CSP violations for inline styles even while parsing into
 * an inert document, so styles are cut textually before DOMPurify parses;
 * whatever this misses is still removed by DOMPurify and never applied.
 */
export function stripStyles(html: string): string {
	return html.replace(STYLE_ELEMENT, '').replace(STYLE_ATTRIBUTE, '');
}

export function sanitizeFragment(purify: DOMPurify, html: string): DocumentFragment {
	return purify.sanitize(stripStyles(html), { ...COMPOSE_PURIFY, RETURN_DOM_FRAGMENT: true });
}
