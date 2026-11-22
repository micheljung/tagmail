export interface Cursor {
	epoch: string;
	seq: number;
}

/** Parses the `<epoch>:<seq>` change cursor; `null` if malformed. */
export function parseCursor(raw: string | null | undefined): Cursor | null {
	if (!raw) return null;
	const at = raw.lastIndexOf(':');
	if (at <= 0) return null;
	const digits = raw.slice(at + 1);
	if (!/^\d+$/.test(digits)) return null;
	const seq = Number(digits);
	if (!Number.isSafeInteger(seq)) return null;
	return { epoch: raw.slice(0, at), seq };
}
