//! APPEND with UIDPLUS `APPENDUID`, from memory or streamed from a reader.

use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::core::Literal;
use imap_next::imap_types::extensions::binary::LiteralOrLiteral8;
use imap_next::imap_types::response::Code;
use tokio::io::AsyncRead;

use crate::ImapSession as _;
use crate::error::{Error, Result};
use crate::session::{TokioSession, check_done, imap_flags};
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
        self.appended(mailbox, out, uid).await
    }

    async fn appended(
        &mut self,
        mailbox: &str,
        out: crate::conn::Collected,
        uid: Option<Uid>,
    ) -> Result<AppendResult> {
        self.split_events(out, |_| false);
        // APPENDUID's UIDVALIDITY is not trusted (see AppendResult).
        let uid_validity = match uid {
            Some(_) => self.status(mailbox).await?.uid_validity,
            None => None,
        };
        Ok(AppendResult { uid, uid_validity })
    }

    pub(crate) async fn append_stream_impl(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        size: u64,
        message: &mut (dyn AsyncRead + Unpin + Send),
    ) -> Result<AppendResult> {
        let quoted = quote(mailbox)?;
        let flags: Vec<&str> = flags.iter().map(Flag::as_imap).collect();
        if flags
            .iter()
            .any(|f| !f.bytes().all(|b| b.is_ascii_graphic()))
        {
            return Err(Error::InvalidArgument(format!("flags {flags:?}")));
        }
        let line = format!("APPEND {quoted} ({})", flags.join(" "));
        self.ensure_usable()?;
        let (out, done) = self.conn.run_with_literal(&line, size, message).await?;
        let done = check_done(done)?;
        let uid = out.codes().chain(done.code.as_ref()).find_map(|c| match c {
            Code::AppendUid { uid, .. } => Some(Uid(uid.get())),
            _ => None,
        });
        self.appended(mailbox, out, uid).await
    }
}

/// `name` as an IMAP quoted string.
fn quote(name: &str) -> Result<String> {
    if name.is_empty() || !name.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Err(Error::InvalidArgument(format!(
            "mailbox {name:?} cannot be quoted"
        )));
    }
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for c in name.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    Ok(out)
}
