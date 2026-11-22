//! Chunked body fetch: `BODY.PEEK[section]<offset.chunk>` streamed into an
//! async writer, so memory stays at about one chunk whatever the size.

use std::num::NonZeroU32;

use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::fetch::{
    MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName, Part, Section,
};
use imap_next::imap_types::response::Data;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::error::{Error, Result};
use crate::fetch::sequence_set;
use crate::session::TokioSession;
use crate::types::{BodyFetch, Uid, UidSet};

/// Parses `""`, `HEADER`, `TEXT`, `1.2`, `1.2.HEADER`, `1.2.TEXT`, `1.2.MIME`.
pub(crate) fn parse_section(section: &str) -> Result<Option<Section<'static>>> {
    if section.is_empty() {
        return Ok(None);
    }
    let invalid = || Error::InvalidArgument(format!("section {section:?}"));
    let mut numbers = Vec::new();
    let mut suffix = None;
    for piece in section.split('.') {
        if suffix.is_some() {
            return Err(invalid());
        }
        match piece.parse::<u32>().ok().and_then(NonZeroU32::new) {
            Some(n) => numbers.push(n),
            None => suffix = Some(piece.to_ascii_uppercase()),
        }
    }
    let part = (!numbers.is_empty()).then(|| Part(numbers.try_into().expect("not empty")));
    Ok(Some(match (part, suffix.as_deref()) {
        (Some(p), None) => Section::Part(p),
        (p, Some("HEADER")) => Section::Header(p),
        (p, Some("TEXT")) => Section::Text(p),
        (Some(p), Some("MIME")) => Section::Mime(p),
        _ => return Err(invalid()),
    }))
}

impl TokioSession {
    pub(crate) async fn fetch_body_impl(
        &mut self,
        uid: Uid,
        section: &str,
        sink: &mut (dyn AsyncWrite + Unpin + Send),
        max_bytes: u64,
    ) -> Result<BodyFetch> {
        self.selected_state()?;
        let section = parse_section(section)?;
        let chunk = NonZeroU32::new(self.body_chunk)
            .ok_or_else(|| Error::InvalidArgument("body chunk 0".into()))?;
        let set = sequence_set(&[uid].into_iter().collect::<UidSet>())?;
        let mut offset: u32 = 0;
        let mut written: u64 = 0;
        loop {
            let body = CommandBody::Fetch {
                sequence_set: set.clone(),
                macro_or_item_names: MacroOrMessageDataItemNames::MessageDataItemNames(vec![
                    MessageDataItemName::Uid,
                    MessageDataItemName::BodyExt {
                        section: section.clone(),
                        partial: Some((offset, chunk)),
                        peek: true,
                    },
                ]),
                uid: true,
                modifiers: Vec::new(),
            };
            let (mut out, _) = self.run(body).await?;
            if out.too_long {
                return Err(Error::ResponseTooLong {
                    limit: self.conn.max_response_size,
                });
            }
            let mut data = None;
            out.data.retain(|d| {
                let Data::Fetch { items, .. } = d else {
                    return true;
                };
                let items = items.as_ref();
                let ours = items
                    .iter()
                    .any(|i| matches!(i, MessageDataItem::Uid(u) if u.get() == uid.0));
                let body = items.iter().find_map(|i| match i {
                    MessageDataItem::BodyExt { data, .. } => Some(data),
                    _ => None,
                });
                match body {
                    Some(b) if ours => {
                        data = Some(
                            b.0.as_ref()
                                .map(|s| s.as_ref().to_vec())
                                .unwrap_or_default(),
                        );
                        false
                    }
                    _ => true,
                }
            });
            self.split_events(out, |_| false);
            let data = data.ok_or(Error::NotFound)?;
            let remaining = max_bytes - written;
            let take = data
                .len()
                .min(usize::try_from(remaining).unwrap_or(usize::MAX));
            sink.write_all(&data[..take]).await?;
            written += take as u64;
            let complete = data.len() < chunk.get() as usize;
            if take < data.len() || (written == max_bytes && !complete) {
                sink.flush().await?;
                return Ok(BodyFetch {
                    bytes: written,
                    truncated: true,
                });
            }
            if complete {
                sink.flush().await?;
                return Ok(BodyFetch {
                    bytes: written,
                    truncated: false,
                });
            }
            offset = offset
                .checked_add(chunk.get())
                .ok_or_else(|| Error::Protocol("message larger than 4 GiB".into()))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections() {
        assert_eq!(parse_section("").unwrap(), None);
        assert!(matches!(
            parse_section("1.2").unwrap(),
            Some(Section::Part(_))
        ));
        assert!(matches!(
            parse_section("header").unwrap(),
            Some(Section::Header(None))
        ));
        assert!(matches!(
            parse_section("2.TEXT").unwrap(),
            Some(Section::Text(Some(_)))
        ));
        assert!(matches!(
            parse_section("2.MIME").unwrap(),
            Some(Section::Mime(_))
        ));
        assert!(parse_section("MIME").is_err());
        assert!(parse_section("1.TEXT.2").is_err());
        assert!(parse_section("0").is_err());
    }
}
