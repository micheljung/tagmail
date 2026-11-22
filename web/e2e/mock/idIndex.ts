/** Encoder twin of `decodeIdIndex` (crates/api-core/src/id_index.rs). */
export function encodeIdIndex(ids: readonly number[]): Uint8Array<ArrayBuffer> {
	const out: number[] = [];
	let prev = 0;
	for (const id of ids) {
		const delta = id - prev;
		prev = id;
		const negative = delta < 0;
		let magnitude = negative ? -delta - 1 : delta;
		let byte = ((magnitude % 64) << 1) | (negative ? 1 : 0);
		magnitude = Math.floor(magnitude / 64);
		while (magnitude > 0) {
			out.push(byte | 0x80);
			byte = magnitude % 128;
			magnitude = Math.floor(magnitude / 128);
		}
		out.push(byte);
	}
	return Uint8Array.from(out);
}
