//! What the handlers need from the outside: a [`Store`] (the server's SQLite
//! databases, the desktop's local-first composite) and an
//! [`AccountRegistry`] (the sync engine).

use std::fmt;
use std::future::Future;
use std::str::FromStr;

use tagmail_core::query::{self, Query, QueryError};
use tagmail_core::{AccountId, ChangeCursor, LabelKey, MailboxRowId, ThreadId};
use zeroize::Zeroizing;

use crate::dto::{ChangeDto, LabelDto, MailboxDto, ThreadRowDto, TlsMode};

pub type StoreError = Box<dyn std::error::Error + Send + Sync>;
pub type StoreResult<T> = Result<T, StoreError>;

/// A thread list. Path form: `inbox` (every account's INBOX),
/// `mailbox:<id>`, `label:<label key>` or `search:<query>`; label and search
/// views exclude Trash and Junk unless the query names a location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Inbox,
    Mailbox(MailboxRowId),
    Label(LabelKey),
    Search(Query),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    Unknown,
    Query(QueryError),
}

impl FromStr for View {
    type Err = ViewError;

    fn from_str(s: &str) -> Result<View, ViewError> {
        match s.split_once(':') {
            None if s == "inbox" => Ok(View::Inbox),
            Some(("mailbox", id)) if id.bytes().all(|b| b.is_ascii_digit()) => id
                .parse()
                .map(|id| View::Mailbox(MailboxRowId(id)))
                .map_err(|_| ViewError::Unknown),
            Some(("label", key)) if !key.trim().is_empty() => Ok(View::Label(LabelKey::of(key))),
            Some(("search", q)) => query::parse(q).map(View::Search).map_err(ViewError::Query),
            _ => Err(ViewError::Unknown),
        }
    }
}

/// Thread ids of a view, newest first, as of `cursor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewIndex {
    pub cursor: ChangeCursor,
    pub ids: Vec<ThreadId>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRows {
    pub cursor: ChangeCursor,
    pub rows: Vec<ThreadRowDto>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ChangePage {
    Page {
        changes: Vec<ChangeDto>,
        next: ChangeCursor,
        more: bool,
    },
    /// Cursor from another cache epoch or below the trim floor.
    Gone,
}

/// Per-account persisted state for `/api/status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    pub id: AccountId,
    pub name: String,
    /// `account.status`; anything but `active` means the engine does not
    /// sync it.
    pub status: String,
    pub mailboxes_initial: u32,
    pub mailboxes_catchup: u32,
    pub mailboxes_live: u32,
    /// Messages the text pass has yet to fetch.
    pub text_pending: u32,
}

/// A credential in memory: zeroed on drop, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<String>);

impl Secret {
    pub fn new(value: String) -> Secret {
        Secret(Zeroizing::new(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// A validated account to persist. The store encrypts `password`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAccount {
    pub name: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_tls: TlsMode,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_tls: TlsMode,
    pub username: String,
    pub password: Secret,
    pub max_connections: u8,
}

/// Read access to mail data and the change log, plus account persistence.
pub trait Store: Send + Sync + 'static {
    /// Cursor of the newest change.
    fn head(&self) -> impl Future<Output = StoreResult<ChangeCursor>> + Send;

    /// The ordered thread ids of `view` and the cursor they are current at,
    /// read in one snapshot.
    fn view_index(&self, view: View) -> impl Future<Output = StoreResult<ViewIndex>> + Send;

    /// Mailboxes of every account.
    fn mailboxes(&self) -> impl Future<Output = StoreResult<Vec<MailboxDto>>> + Send;

    /// Labels of every account, one per key.
    fn labels(&self) -> impl Future<Output = StoreResult<Vec<LabelDto>>> + Send;

    /// Rows for those of `ids` that exist and are not merged, in `ids`
    /// order, read in one snapshot with their cursor.
    fn threads(&self, ids: Vec<ThreadId>) -> impl Future<Output = StoreResult<ThreadRows>> + Send;

    /// Up to `limit` changes after `since`.
    fn changes_since(
        &self,
        since: ChangeCursor,
        limit: usize,
    ) -> impl Future<Output = StoreResult<ChangePage>> + Send;

    fn accounts(&self) -> impl Future<Output = StoreResult<Vec<AccountSummary>>> + Send;

    /// Persists the account with its password encrypted; returns its id.
    fn create_account(
        &self,
        account: NewAccount,
    ) -> impl Future<Output = StoreResult<AccountId>> + Send;
}

/// Engine state that only lives in memory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeStatus {
    pub last_error: Option<String>,
    /// IMAP capabilities the engine relies on, e.g. `QRESYNC`.
    pub capabilities: Vec<String>,
    /// Missing features that weaken sync, e.g. `no_modseq:Archive`.
    pub degraded: Vec<String>,
}

/// The sync engine as seen by the API.
pub trait AccountRegistry: Send + Sync + 'static {
    /// Called after a new account is committed; the engine starts syncing it
    /// in the background and reports problems through [`Self::runtime_status`].
    fn account_added(&self, id: AccountId);

    /// `None` if the engine has not run the account yet.
    fn runtime_status(&self, id: AccountId) -> Option<RuntimeStatus>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_parses_path_forms() {
        assert_eq!("inbox".parse(), Ok(View::Inbox));
        assert_eq!("mailbox:42".parse(), Ok(View::Mailbox(MailboxRowId(42))));
        assert_eq!("label:Work".parse(), Ok(View::Label(LabelKey::of("work"))));
        assert_eq!(
            "search:a/b:c".parse(),
            Ok(View::Search(query::parse("a/b:c").unwrap()))
        );
        assert_eq!(
            "search:(".parse::<View>(),
            Err(ViewError::Query(QueryError::Unbalanced(0)))
        );
        for bad in [
            "",
            "Inbox",
            "mailbox:",
            "mailbox:-1",
            "mailbox:+1",
            "label:",
            "x:y",
        ] {
            assert_eq!(bad.parse::<View>(), Err(ViewError::Unknown), "{bad}");
        }
    }

    #[test]
    fn secret_debug_is_redacted() {
        let s = Secret::new("hunter2".to_owned());
        assert!(!format!("{s:?}").contains("hunter2"));
    }
}
