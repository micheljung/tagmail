import type { AccountDto } from '@tagmail/api-types';
import { describe, expect, it } from 'vitest';
import { formOf, needsPassword, type AccountForm } from './accountForm';

const account: AccountDto = {
	id: 1,
	name: 'Personal',
	email: 'me@example.com',
	status: 'active',
	imapHost: 'imap.example.com',
	imapPort: 993,
	imapTls: 'tls',
	smtpHost: 'smtp.example.com',
	smtpPort: 465,
	smtpTls: 'tls',
	username: 'me@example.com',
	maxConnections: 4
};

const edited = (change: Partial<AccountForm>) => ({ ...formOf(account), ...change });

describe('needsPassword', () => {
	it('is false for an untouched form and for harmless edits', () => {
		expect(needsPassword(formOf(account), account)).toBe(false);
		expect(needsPassword(edited({ name: 'Other', maxConnections: 2 }), account)).toBe(false);
	});

	it.each([
		{ imapHost: 'imap.evil.example' },
		{ imapPort: 143 },
		{ imapTls: 'starttls' },
		{ smtpHost: 'smtp.evil.example' },
		{ smtpPort: 587 },
		{ smtpTls: 'starttls' },
		{ username: 'other@example.com' }
	] satisfies Partial<AccountForm>[])('is true when %o changes', (change) => {
		expect(needsPassword(edited(change), account)).toBe(true);
	});

	it('ignores surrounding whitespace', () => {
		expect(needsPassword(edited({ imapHost: ' imap.example.com ' }), account)).toBe(false);
	});
});
