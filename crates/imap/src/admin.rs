//! Mailbox operations used by `cargo xtask probe-server`, not by the engine.

use imap_next::imap_types::command::CommandBody;

use crate::error::Result;
use crate::session::TokioSession;

impl TokioSession {
    /// Deletes `name` with its messages. It must not be the selected mailbox.
    pub async fn delete_mailbox(&mut self, name: &str) -> Result<()> {
        self.run(CommandBody::Delete {
            mailbox: Self::mailbox(name)?,
        })
        .await?;
        Ok(())
    }
}
