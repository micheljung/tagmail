import type { AccountDto, AccountPatchDto, NewAccountDto, TlsMode } from '@tagmail/api-types';

/** Implicit TLS and STARTTLS ports, swapped in when the mode changes and the port was a default. */
export const PORTS = {
	imap: { tls: 993, starttls: 143 },
	smtp: { tls: 465, starttls: 587 }
} as const satisfies Record<'imap' | 'smtp', Record<TlsMode, number>>;

export const DEFAULT_MAX_CONNECTIONS = 4;

export interface AccountForm {
	name: string;
	email: string;
	displayName: string;
	username: string;
	password: string;
	imapHost: string;
	imapPort: number;
	imapTls: TlsMode;
	smtpHost: string;
	smtpPort: number;
	smtpTls: TlsMode;
	maxConnections: number;
}

export function emptyForm(): AccountForm {
	return {
		name: '',
		email: '',
		displayName: '',
		username: '',
		password: '',
		imapHost: '',
		imapPort: PORTS.imap.tls,
		imapTls: 'tls',
		smtpHost: '',
		smtpPort: PORTS.smtp.tls,
		smtpTls: 'tls',
		maxConnections: DEFAULT_MAX_CONNECTIONS
	};
}

export function formOf(a: AccountDto): AccountForm {
	return {
		name: a.name,
		email: a.email ?? '',
		displayName: '',
		username: a.username,
		password: '',
		imapHost: a.imapHost,
		imapPort: a.imapPort,
		imapTls: a.imapTls,
		smtpHost: a.smtpHost,
		smtpPort: a.smtpPort,
		smtpTls: a.smtpTls,
		maxConnections: a.maxConnections
	};
}

/** The address to send from, guessed from a username that is one. */
export function emailFromUsername(username: string): string {
	return /^[^\s@]+@[^\s@]+$/.test(username.trim()) ? username.trim() : '';
}

export function newAccountOf(f: AccountForm): NewAccountDto {
	const dto: NewAccountDto = {
		name: f.name.trim(),
		imapHost: f.imapHost.trim(),
		imapPort: f.imapPort,
		imapTls: f.imapTls,
		smtpHost: f.smtpHost.trim(),
		smtpPort: f.smtpPort,
		smtpTls: f.smtpTls,
		username: f.username.trim(),
		password: f.password,
		maxConnections: f.maxConnections
	};
	if (f.email.trim()) dto.email = f.email.trim();
	if (f.displayName.trim()) dto.displayName = f.displayName.trim();
	return dto;
}

/** Only the fields that differ from `a`; an empty password keeps the stored one. */
export function patchOf(f: AccountForm, a: AccountDto): AccountPatchDto {
	const next = newAccountOf(f);
	const patch: AccountPatchDto = {};
	if (next.name !== a.name) patch.name = next.name;
	if ((next.email ?? '') !== (a.email ?? '') && next.email) patch.email = next.email;
	if (next.imapHost !== a.imapHost) patch.imapHost = next.imapHost;
	if (next.imapPort !== a.imapPort) patch.imapPort = next.imapPort;
	if (next.imapTls !== a.imapTls) patch.imapTls = next.imapTls;
	if (next.smtpHost !== a.smtpHost) patch.smtpHost = next.smtpHost;
	if (next.smtpPort !== a.smtpPort) patch.smtpPort = next.smtpPort;
	if (next.smtpTls !== a.smtpTls) patch.smtpTls = next.smtpTls;
	if (next.username !== a.username) patch.username = next.username;
	if (next.maxConnections !== a.maxConnections) patch.maxConnections = next.maxConnections;
	if (f.password) patch.password = f.password;
	return patch;
}
