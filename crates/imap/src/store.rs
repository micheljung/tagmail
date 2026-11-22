//! `UID STORE` of flags and keywords.

use imap_next::imap_types::command::{CommandBody, StoreModifier};
use imap_next::imap_types::flag::{StoreResponse, StoreType};
use imap_next::imap_types::response::Code;

use crate::error::Result;
use crate::fetch::{sequence_set, uid_set};
use crate::session::{TokioSession, imap_flags};
use crate::types::{Flag, ModSeq, StoreOp, StoreResult, UidSet};

impl TokioSession {
    pub(crate) async fn store_impl(
        &mut self,
        uids: &UidSet,
        op: StoreOp,
        flags: &[Flag],
        unchanged_since: Option<ModSeq>,
    ) -> Result<StoreResult> {
        self.selected_state()?;
        if uids.is_empty() {
            return Ok(StoreResult::default());
        }
        let mut modifiers = Vec::new();
        if let Some(m) = unchanged_since
            && self.caps.condstore()
        {
            modifiers.push(StoreModifier::UnchangedSince(m.0));
        }
        let body = CommandBody::Store {
            sequence_set: sequence_set(uids)?,
            kind: match op {
                StoreOp::Add => StoreType::Add,
                StoreOp::Remove => StoreType::Remove,
                StoreOp::Replace => StoreType::Replace,
            },
            response: StoreResponse::Silent,
            flags: imap_flags(flags)?,
            uid: true,
            modifiers,
        };
        let (out, done) = self.run(body).await?;
        let mut modified = UidSet::new();
        for code in out.codes().chain(done.code.as_ref()) {
            if let Code::Modified(set) = code {
                modified.extend(&uid_set(set));
            }
        }
        let (updates, _) = self.split_events(out, |m| m.uid.is_some_and(|u| uids.contains(u)));
        Ok(StoreResult { modified, updates })
    }
}
