import { ApiError } from '../transport';

const MESSAGES: Record<string, string> = {
	invalid_credentials: 'That password is not right.',
	unauthorized: 'That password is not right.',
	rate_limited: 'Too many attempts. Wait a minute and try again.',
	invalid_setup_token: 'That setup token is not valid. Copy it again from the server log.',
	already_set_up: 'This server is already set up. Sign in instead.',
	no_passkeys: 'No passkey is registered yet. Sign in with your password, then add one.',
	passkeys_unavailable: 'Passkeys need the server’s host name, not an IP address.',
	csrf: 'The request was refused as cross-site. Reload the page and try again.'
};

const HTTP_TOO_MANY_REQUESTS = 429;

/** Player-facing text for an API or WebAuthn failure. */
export function errorText(e: unknown): string {
	if (e instanceof ApiError) {
		if (e.status === HTTP_TOO_MANY_REQUESTS) return MESSAGES.rate_limited;
		return MESSAGES[e.code] ?? e.message;
	}
	if (e instanceof DOMException && e.name === 'NotAllowedError') {
		return 'The passkey prompt was closed before it finished.';
	}
	return e instanceof Error ? e.message : String(e);
}
