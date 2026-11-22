import { describe, expect, it } from 'vitest';
import { SEQUENCE_TIMEOUT_MS, ShortcutMatcher, type KeyLike } from './keymap';

function key(k: string, extra: Partial<KeyLike> = {}): KeyLike {
	return { key: k, ctrlKey: false, metaKey: false, altKey: false, target: null, ...extra };
}

function element(tagName: string, props: Record<string, unknown> = {}): EventTarget {
	return {
		tagName,
		isContentEditable: false,
		closest: (sel: string) => (sel.includes(tagName.toLowerCase()) ? {} : null),
		...props
	} as unknown as EventTarget;
}

describe('ShortcutMatcher', () => {
	it('maps the read shortcuts', () => {
		const m = new ShortcutMatcher();
		expect(['j', 'k', 'o', 'Enter', 'u', '/', '?'].map((k) => m.match(key(k)))).toEqual([
			'next',
			'previous',
			'open',
			'open',
			'back',
			'search',
			'help'
		]);
	});

	it('matches g sequences within the timeout', () => {
		const now = 0;
		const m = new ShortcutMatcher(() => now);
		expect(m.match(key('g'))).toBeNull();
		expect(m.pending).toBe('g');
		expect(m.match(key('i'))).toBe('goInbox');
		expect(m.match(key('g'))).toBeNull();
		expect(m.match(key('l'))).toBe('goLabel');
	});

	it('drops a stale prefix and treats the key on its own', () => {
		let now = 0;
		const m = new ShortcutMatcher(() => now);
		m.match(key('g'));
		now = SEQUENCE_TIMEOUT_MS + 1;
		expect(m.match(key('j'))).toBe('next');
		m.match(key('g'));
		expect(m.match(key('x'))).toBeNull();
		expect(m.pending).toBeNull();
	});

	it('ignores keys with modifiers or while composing', () => {
		const m = new ShortcutMatcher();
		expect(m.match(key('j', { ctrlKey: true }))).toBeNull();
		expect(m.match(key('k', { metaKey: true }))).toBeNull();
		expect(m.match(key('j', { isComposing: true }))).toBeNull();
	});

	it('stays out of text fields except for Escape', () => {
		const m = new ShortcutMatcher();
		const input = element('INPUT', { type: 'text' });
		expect(m.match(key('j', { target: input }))).toBeNull();
		expect(m.match(key('/', { target: element('TEXTAREA') }))).toBeNull();
		expect(m.match(key('Escape', { target: input }))).toBe('escape');
		expect(m.match(key('j', { target: element('INPUT', { type: 'checkbox' }) }))).toBe('next');
	});

	it('leaves Enter to focused buttons and links', () => {
		const m = new ShortcutMatcher();
		expect(m.match(key('Enter', { target: element('BUTTON') }))).toBeNull();
		expect(m.match(key('Enter', { target: element('DIV') }))).toBe('open');
	});
});
