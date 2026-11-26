import { describe, expect, it } from 'vitest';
import { listMeta, type ListNames } from './listMeta';

const names: ListNames = {
	account: (id) => `Acct${id}`,
	label: (key) => key.toUpperCase(),
	mailbox: (id) => ({
		name: `Box${id}`,
		specialUse: id === 1 ? 'trash' : id === 2 ? 'junk' : id === 3 ? 'drafts' : null
	})
};

describe('listMeta', () => {
	it('describes the inbox', () => {
		expect(listMeta('inbox', names)).toEqual({
			title: 'Inbox',
			label: 'Inbox',
			emptyTitle: "You're all caught up",
			emptyHint: 'New conversations land here as soon as they sync.'
		});
		expect(listMeta('account:4/inbox', names).label).toBe('Inbox of Acct4');
	});

	it('describes labels', () => {
		const m = listMeta('account:2/label:work', names);
		expect(m.title).toBe('WORK');
		expect(m.label).toBe('Label WORK in Acct2');
		expect(m.emptyTitle).toBe('Nothing labeled WORK');
	});

	it('describes mailboxes by special use', () => {
		expect(listMeta('mailbox:1', names).emptyTitle).toBe('Trash is empty');
		expect(listMeta('mailbox:2', names).emptyTitle).toBe('No spam here');
		expect(listMeta('mailbox:3', names).emptyTitle).toBe('No drafts');
		const m = listMeta('mailbox:9', names);
		expect(m.emptyTitle).toBe('Box9 is empty');
		expect(m.label).toBe('Folder Box9');
		expect(m.title).toBe('Box9');
	});

	it('describes searches', () => {
		const m = listMeta('account:5/search:cats', names);
		expect(m.title).toBe('cats · Search');
		expect(m.label).toBe('Search results for cats in Acct5');
		expect(m.emptyTitle).toBe('No conversations match');
	});
});
