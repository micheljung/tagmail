//! APPEND with UIDPLUS `APPENDUID`.

use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::core::Literal;
use imap_next::imap_types::extensions::binary::LiteralOrLiteral8;
use imap_next::imap_types::response::Code;

use crate::ImapSession as _;
use crate::error::{Error, Result};
use crate::session::{TokioSession, imap_flags};
use crate::types::{AppendResult, Flag, Uid};

impl TokioSession {
    pub(crate) async fn append_impl(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        message: &[u8],
    ) -> Result<AppendResult> {
        let literal = Literal::try_from(message.to_vec())
            .map_err(|e| Error::InvalidArgument(format!("message: {e}")))?;
        let body = CommandBody::Append {
            mailbox: Self::mailbox(mailbox)?,
            flags: imap_flags(flags)?,
            date: None,
            message: LiteralOrLiteral8::Literal(literal),
        };
        let (out, done) = self.run(body).await?;
        let uid = out.codes().chain(done.code.as_ref()).find_map(|c| match c {
            Code::AppendUid { uid, .. } => Some(Uid(uid.get())),
            _ => None,
        });
        self.split_events(out, |_| false);
        // APPENDUID's UIDVALIDITY is not trusted (see AppendResult).
        let uid_validity = match uid {
            Some(_) => self.status(mailbox).await?.uid_validity,
            None => None,
        };
        Ok(AppendResult { uid, uid_validity })
    }
}
