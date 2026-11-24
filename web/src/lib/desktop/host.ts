/** The host of a server address as the browser resolves it (IDNs in punycode). */
export function serverHost(server: string): string {
	try {
		return new URL(server).host;
	} catch {
		return server;
	}
}

/** What the app binds unsent changes to: scheme, host, port and path, like the replica's server identity. */
export function serverIdentity(server: string): string {
	try {
		const url = new URL(server);
		return url.origin + url.pathname.replace(/\/+$/, '');
	} catch {
		return server;
	}
}
