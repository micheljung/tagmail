// Decoder for the binary thread-id index of GET /api/views/{view}/index:
// zigzag-encoded deltas as unsigned LEB128 varints (crates/api-core
// src/id_index.rs). The zigzag value of a 53-bit delta exceeds 2^53, so the
// sign bit is split off the first byte and only the magnitude (< 2^53) is
// accumulated, which keeps every step exact in a double.

const PAYLOAD = 0x7f;
const CONTINUE = 0x80;
const PAYLOAD_RADIX = 128;
const MAX_VARINT_BYTES = 8;

export function decodeIdIndex(bytes: Uint8Array): number[] {
	const ids: number[] = [];
	let prev = 0;
	let pos = 0;
	while (pos < bytes.length) {
		let negative = false;
		let magnitude = 0;
		let scale = 1;
		let count = 0;
		for (;;) {
			if (pos >= bytes.length) throw new Error('id index ends inside a varint');
			if (count === MAX_VARINT_BYTES) throw new Error('id index varint is too long');
			const byte = bytes[pos++];
			const payload = byte & PAYLOAD;
			if (count === 0) {
				negative = (payload & 1) === 1;
				magnitude = payload >>> 1;
				scale = PAYLOAD_RADIX / 2;
			} else {
				magnitude += payload * scale;
				scale *= PAYLOAD_RADIX;
			}
			count++;
			if ((byte & CONTINUE) === 0) break;
		}
		const delta = negative ? -(magnitude + 1) : magnitude;
		const id = prev + delta;
		if (id < 0 || id > Number.MAX_SAFE_INTEGER) throw new Error(`id ${id} is out of range`);
		ids.push(id);
		prev = id;
	}
	return ids;
}
