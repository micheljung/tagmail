/** The host of a server address as the browser resolves it (IDNs in punycode). */
export function serverHost(server: string): string {
	try {
		return new URL(server).host;
	} catch {
		return server;
	}
}
