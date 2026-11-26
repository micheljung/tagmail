import { describe, expect, it } from 'vitest';
import { scopedView, viewForPath, withAccount } from './views';

describe('viewForPath', () => {
	it('maps the inbox, scoped or not', () => {
		expect(viewForPath('/inbox')).toEqual({ view: 'inbox', path: '/inbox' });
		expect(viewForPath('/inbox?account=3')).toEqual({
			view: scopedView('inbox', 3),
			path: withAccount('/inbox', 3)
		});
	});

	it('maps labels and decodes their key', () => {
		expect(viewForPath('/label/Work%20%26%20Home?account=2')).toEqual({
			view: 'account:2/label:Work & Home',
			path: '/label/Work%20%26%20Home?account=2'
		});
	});

	it('maps mailboxes', () => {
		expect(viewForPath('/mailbox/7')).toEqual({ view: 'mailbox:7', path: '/mailbox/7' });
		expect(viewForPath('/mailbox/x')).toBeNull();
	});

	it('maps searches and ignores empty ones', () => {
		expect(viewForPath('/search?q=hello%20world')).toEqual({
			view: 'search:hello world',
			path: '/search?q=hello%20world'
		});
		expect(viewForPath('/search?q=%20')).toBeNull();
		expect(viewForPath('/search')).toBeNull();
	});

	it('accepts a URL object and rejects other paths', () => {
		expect(viewForPath(new URL('http://x/inbox'))).toEqual({ view: 'inbox', path: '/inbox' });
		expect(viewForPath('/settings/accounts')).toBeNull();
		expect(viewForPath('/thread/1')).toBeNull();
		expect(viewForPath('/label/%E0%A4%A')).toBeNull();
	});
});
