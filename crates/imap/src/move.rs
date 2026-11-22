//! `UID MOVE` with COPY + EXPUNGE fallbacks.

use imap_next::imap_types::command::CommandBody;
use imap_next::imap_types::response::Code;

use crate::ImapSession as _;
use crate::error::{Error, Result};
use crate::fetch::sequence_set;
use crate::session::TokioSession;
use crate::types::{CopyUid, Flag, MoveResult, SearchCriteria, StoreOp, Uid, UidSet, UidValidity};

fn copy_uid<'a>(codes: impl Iterator<Item = &'a Code<'static>>) -> Option<CopyUid> {
    codes.into_iter().find_map(|code| match code {
        Code::CopyUid {
            uid_validity,
            source,
            destination,
        } => {
            let expand = |set: &imap_next::imap_types::extensions::uidplus::UidSet| {
                let mut uids = Vec::new();
                for e in set.0.as_ref() {
                    use imap_next::imap_types::extensions::uidplus::UidElement;
                    match e {
                        UidElement::Single(u) => uids.push(Uid(u.get())),
                        // COPYUID ranges may be descending (RFC 4315 §3).
                        UidElement::Range(a, b) => {
                            let (a, b) = (a.get(), b.get());
                            if a <= b {
                                uids.extend((a..=b).map(Uid));
                            } else {
                                uids.extend((b..=a).rev().map(Uid));
                            }
                        }
                    }
                }
                uids
            };
            Some(CopyUid {
                uid_validity: UidValidity(uid_validity.get()),
                source: expand(source),
                destination: expand(destination),
            })
        }
        _ => None,
    })
}

impl TokioSession {
    pub(crate) async fn move_impl(
        &mut self,
        uids: &UidSet,
        destination: &str,
    ) -> Result<MoveResult> {
        self.selected_state()?;
        if uids.is_empty() {
            return Ok(MoveResult::default());
        }
        let mailbox = Self::mailbox(destination)?;
        let sequence_set = sequence_set(uids)?;
        if self.caps.r#move() {
            let (out, done) = self
                .run(CommandBody::Move {
                    sequence_set,
                    mailbox,
                    uid: true,
                })
                .await?;
            let copy_uid = copy_uid(out.codes().chain(done.code.as_ref()));
            self.split_events(out, |_| false);
            return Ok(MoveResult { copy_uid });
        }
        let uidplus = self.caps.uidplus();
        if !uidplus {
            let deleted = self.uid_search(SearchCriteria::Deleted).await?;
            if !deleted.difference(uids).is_empty() {
                return Err(Error::Unsupported(
                    "move without MOVE or UIDPLUS while other messages are \\Deleted",
                ));
            }
        }
        let (out, done) = self
            .run(CommandBody::Copy {
                sequence_set: sequence_set.clone(),
                mailbox,
                uid: true,
            })
            .await?;
        let copy_uid = copy_uid(out.codes().chain(done.code.as_ref()));
        self.split_events(out, |_| false);
        self.store_impl(uids, StoreOp::Add, &[Flag::Deleted], None)
            .await?;
        let body = if uidplus {
            CommandBody::ExpungeUid { sequence_set }
        } else {
            CommandBody::Expunge
        };
        let (out, _) = self.run(body).await?;
        self.split_events(out, |_| false);
        Ok(MoveResult { copy_uid })
    }
}
