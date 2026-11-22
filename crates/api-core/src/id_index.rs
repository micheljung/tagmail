//! Binary thread-id index (plan §3.11): each id as the zigzag-encoded
//! difference to the previous id (the first to 0), written as an unsigned
//! LEB128 varint. List order is by date, so deltas can be negative. Ids are
//! limited to `0..=MAX_ID` so a JavaScript client can decode them into
//! `number` without loss.

use thiserror::Error;

/// Largest id a JavaScript `number` holds exactly (2^53 − 1).
pub const MAX_ID: i64 = (1 << 53) - 1;

const VARINT_PAYLOAD_BITS: u32 = 7;
const VARINT_CONTINUE: u8 = 0x80;
const VARINT_PAYLOAD: u8 = 0x7f;
/// A zigzag delta of two ids in `0..=MAX_ID` needs at most 55 bits.
const MAX_VARINT_BYTES: u32 = 8;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum IdIndexError {
    #[error("id {0} is outside 0..=2^53-1")]
    IdOutOfRange(i64),
    #[error("index ends inside a varint")]
    Truncated,
    #[error("varint at byte {0} is too long")]
    Overlong(usize),
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

pub fn encode(ids: &[i64]) -> Result<Vec<u8>, IdIndexError> {
    let mut out = Vec::with_capacity(ids.len() * 2);
    let mut prev = 0i64;
    for &id in ids {
        if !(0..=MAX_ID).contains(&id) {
            return Err(IdIndexError::IdOutOfRange(id));
        }
        let mut v = zigzag(id - prev);
        prev = id;
        loop {
            let byte = (v as u8) & VARINT_PAYLOAD;
            v >>= VARINT_PAYLOAD_BITS;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | VARINT_CONTINUE);
        }
    }
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<Vec<i64>, IdIndexError> {
    let mut ids = Vec::new();
    let mut prev = 0i64;
    let mut pos = 0;
    while pos < bytes.len() {
        let start = pos;
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let byte = *bytes.get(pos).ok_or(IdIndexError::Truncated)?;
            pos += 1;
            if shift >= MAX_VARINT_BYTES * VARINT_PAYLOAD_BITS {
                return Err(IdIndexError::Overlong(start));
            }
            v |= u64::from(byte & VARINT_PAYLOAD) << shift;
            shift += VARINT_PAYLOAD_BITS;
            if byte & VARINT_CONTINUE == 0 {
                break;
            }
        }
        // v < 2^56 and prev <= MAX_ID, so the sum cannot overflow.
        let id = prev + unzigzag(v);
        if !(0..=MAX_ID).contains(&id) {
            return Err(IdIndexError::IdOutOfRange(id));
        }
        ids.push(id);
        prev = id;
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn small_deltas_take_one_byte() {
        assert_eq!(encode(&[1, 2, 1, 0]).unwrap(), [2, 2, 1, 1]);
        assert_eq!(encode(&[]).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn extreme_ids_round_trip() {
        let ids = [MAX_ID, 0, MAX_ID, MAX_ID - 1, 1];
        let bytes = encode(&ids).unwrap();
        assert_eq!(decode(&bytes).unwrap(), ids);
    }

    #[test]
    fn rejects_out_of_range_ids() {
        assert_eq!(encode(&[-1]), Err(IdIndexError::IdOutOfRange(-1)));
        assert_eq!(
            encode(&[MAX_ID + 1]),
            Err(IdIndexError::IdOutOfRange(MAX_ID + 1))
        );
        // zigzag(-1) = 1: a first delta of -1 decodes to id -1.
        assert!(matches!(decode(&[1]), Err(IdIndexError::IdOutOfRange(-1))));
    }

    #[test]
    fn rejects_malformed_varints() {
        assert_eq!(decode(&[0x80]), Err(IdIndexError::Truncated));
        assert_eq!(decode(&[2, 0xff, 0xff]), Err(IdIndexError::Truncated));
        assert_eq!(decode(&[0x80; 9]), Err(IdIndexError::Overlong(0)));
    }

    proptest! {
        #[test]
        fn round_trips(ids in proptest::collection::vec(0..=MAX_ID, 0..200)) {
            let bytes = encode(&ids).unwrap();
            prop_assert_eq!(decode(&bytes).unwrap(), ids);
        }

        #[test]
        fn dense_descending_ids_stay_compact(start in 1_000i64..MAX_ID, len in 1usize..500) {
            let ids: Vec<i64> = (0..len as i64).map(|i| start - i).collect();
            let bytes = encode(&ids).unwrap();
            // first id up to 8 bytes, then one byte per -1 step
            prop_assert!(bytes.len() <= len + 7);
            prop_assert_eq!(decode(&bytes).unwrap(), ids);
        }
    }
}
