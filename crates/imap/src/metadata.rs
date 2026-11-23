//! RFC 5464 server annotations (`GETMETADATA` / `SETMETADATA` on `""`).

use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::core::{AString, NString, NString8, Vec1};
use imap_next::imap_types::extensions::metadata::{Entry, EntryValue, MetadataResponse};
use imap_next::imap_types::response::Data;

use crate::error::{Error, Result};
use crate::session::TokioSession;

fn entry(name: &str) -> Result<Entry<'static>> {
    let astring = AString::try_from(name.to_owned())
        .map_err(|e| Error::InvalidArgument(format!("entry {name:?}: {e}")))?;
    Entry::try_from(astring).map_err(|e| Error::InvalidArgument(format!("entry {name:?}: {e}")))
}

fn value_bytes(value: NString8<'_>) -> Option<Vec<u8>> {
    match value {
        NString8::NString(n) => n.into_option().map(|v| v.into_owned()),
        NString8::Literal8(l) => Some(l.data.into_owned()),
    }
}

impl TokioSession {
    pub(crate) async fn get_metadata_impl(&mut self, name: &str) -> Result<Option<Vec<u8>>> {
        if !self.caps.metadata() {
            return Err(Error::Unsupported("METADATA"));
        }
        let wanted = entry(name)?;
        let (out, _) = self
            .run(CommandBody::GetMetadata {
                options: Vec::new(),
                mailbox: Self::mailbox("")?,
                entries: Vec1::from(wanted.clone()),
            })
            .await?;
        for data in out.data {
            if let Data::Metadata {
                items: MetadataResponse::WithValues(values),
                ..
            } = data
            {
                for ev in values.into_inner() {
                    if ev.entry.as_ref().eq_ignore_ascii_case(wanted.as_ref()) {
                        return Ok(value_bytes(ev.value));
                    }
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn set_metadata_impl(
        &mut self,
        name: &str,
        value: Option<&[u8]>,
    ) -> Result<()> {
        if !self.caps.metadata() {
            return Err(Error::Unsupported("METADATA"));
        }
        let value = match value {
            None => NString8::NString(NString::NIL),
            Some(bytes) => NString8::NString(
                NString::try_from(bytes.to_vec())
                    .map_err(|e| Error::InvalidArgument(format!("metadata value: {e}")))?,
            ),
        };
        self.run(CommandBody::SetMetadata {
            mailbox: Self::mailbox("")?,
            entry_values: Vec1::from(EntryValue {
                entry: entry(name)?,
                value,
            }),
        })
        .await?;
        Ok(())
    }
}
