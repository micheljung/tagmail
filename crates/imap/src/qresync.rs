//! CONDSTORE/QRESYNC (RFC 7162) responses.

use imap_next::imap_types::response::Data;

use crate::fetch::uid_set;
use crate::types::Vanished;

/// `* VANISHED [(EARLIER)] <uids>`.
pub(crate) fn vanished(data: &Data<'_>) -> Option<Vanished> {
    match data {
        Data::Vanished {
            earlier,
            known_uids,
        } => Some(Vanished {
            earlier: *earlier,
            uids: uid_set(known_uids),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use imap_codec::ResponseCodec;
    use imap_codec::decode::Decoder;
    use imap_next::imap_types::response::Response;

    use super::*;

    fn parse(line: &[u8]) -> Option<Vanished> {
        match ResponseCodec::default().decode(line).expect("decodes").1 {
            Response::Data(d) => vanished(&d),
            _ => None,
        }
    }

    #[test]
    fn vanished_earlier_and_live() {
        let v = parse(b"* VANISHED (EARLIER) 1:3,5,300:302\r\n").unwrap();
        assert!(v.earlier);
        assert_eq!(v.uids.to_string(), "1:3,5,300:302");
        let v = parse(b"* VANISHED 2\r\n").unwrap();
        assert!(!v.earlier);
        assert_eq!(v.uids.len(), 1);
        assert_eq!(parse(b"* 3 EXPUNGE\r\n"), None);
    }
}
