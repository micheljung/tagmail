import { describe, expect, it } from 'vitest';
import { documentTitle } from './document-title';

describe('documentTitle', () => {
	it('shows only the app name when nothing is unread', () => {
		expect(documentTitle(0)).toBe('Tagmail');
	});

	it('prefixes the unread count', () => {
		expect(documentTitle(3)).toBe('(3) Tagmail');
	});
});
