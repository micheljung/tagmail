//! What the handlers need from the outside: a [`Store`] (the server's SQLite
//! databases, the desktop's local-first composite) and an
//! [`AccountRegistry`] (the sync engine).

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;

use tagmail_core::query::{self, Query, QueryError};
use tagmail_core::{AccountId, ChangeCursor, LabelKey, MailboxRowId, MessageRowId, ThreadId};
use zeroize::Zeroizing;

use crate::dto::{
    AccountDto, AccountStateDto, AccountTestDto, AddressDto, ChangeDto, LabelDto, MailboxDto,
    TestStepDto, ThreadRowDto, TlsMode,
};
use crate::signing::UrlKeys;

pub type StoreError = Box<dyn std::error::Error + Send + Sync>;
pub type StoreResult<T> = Result<T, StoreError>;

/// A thread list. Path form: `inbox` (every account's INBOX),
/// `mailbox:<id>`, `label:<label key>` or `search:<query>`; label and search
/// views exclude Trash and Junk unless the query names a location.
/// `account:<id>/<view>` limits an inbox, label or search view to one
/// account; unscoped views span every account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    Inbox,
    Mailbox(MailboxRowId),
    Label(LabelKey),
    Search(Query),
    Account(AccountId, Box<View>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewError {
    Unknown,
    Query(QueryError),
    /// An account scope around a view that cannot have one.
    Scope,
}

const ACCOUNT_SCOPE: &str = "account:";

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

impl FromStr for View {
    type Err = ViewError;

    fn from_str(s: &str) -> Result<View, ViewError> {
        if let Some(scoped) = s.strip_prefix(ACCOUNT_SCOPE) {
            let (id, inner) = scoped.split_once('/').ok_or(ViewError::Unknown)?;
            if !digits(id) {
                return Err(ViewError::Unknown);
            }
            let id = id.parse().map_err(|_| ViewError::Unknown)?;
            return match inner.parse()? {
                View::Mailbox(_) | View::Account(..) => Err(ViewError::Scope),
                inner => Ok(View::Account(AccountId(id), Box::new(inner))),
            };
        }
        match s.split_once(':') {
            None if s == "inbox" => Ok(View::Inbox),
            Some(("mailbox", id)) if digits(id) => id
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
    pub email: Option<String>,
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

/// A validated account to persist. The store encrypts `password` and
/// creates the default identity (`email`, `display_name`) in the same
/// transaction.
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
    pub email: String,
    pub display_name: Option<String>,
}

/// Outcome of [`Store::mark_deleting`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkDeleting {
    Marked,
    AlreadyDeleting,
    Missing,
}

/// A validated `PATCH /api/accounts/{id}`: the fields to change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccountPatch {
    pub name: Option<String>,
    pub email: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_tls: Option<TlsMode>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_tls: Option<TlsMode>,
    pub username: Option<String>,
    pub password: Option<Secret>,
    pub max_connections: Option<u8>,
    /// `Active` or `Disabled`.
    pub status: Option<AccountStateDto>,
}

/// A message's cached metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    pub id: MessageRowId,
    pub account_id: AccountId,
    pub thread_id: ThreadId,
    /// Unix seconds.
    pub date: i64,
    pub subject: Option<String>,
    pub snippet: String,
    pub from: Vec<AddressDto>,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub unread: bool,
    pub starred: bool,
    pub has_attachment: bool,
}

/// A thread with its live messages, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadDetail {
    pub row: ThreadRowDto,
    pub messages: Vec<MessageRecord>,
}

/// Why a raw message could not be fetched on open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchRawError {
    /// The server no longer has the message.
    NotFound,
    /// Larger than the raw size cap.
    TooLarge,
    /// No connection, the account is not syncing, or the fetch failed.
    Unavailable(String),
}

impl fmt::Display for FetchRawError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FetchRawError::NotFound => f.write_str("the message is gone from the server"),
            FetchRawError::TooLarge => f.write_str("the message is too large to display"),
            FetchRawError::Unavailable(why) => write!(f, "the message is unavailable: {why}"),
        }
    }
}

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// A remote image fetched by the image proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxiedImage {
    /// Sniffed from the bytes, not taken from the origin.
    pub content_type: &'static str,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyError {
    /// The URL, a redirect or a resolved address is not allowed.
    Blocked(String),
    /// The origin failed or answered with something that is not an image.
    Upstream(String),
    TooLarge,
    Timeout,
}

/// Fetches remote images for display (`/api/proxy/img/{token}`); the
/// server's implementation guards against SSRF.
pub trait ImageProxy: Send + Sync + 'static {
    fn fetch(&self, url: String) -> BoxFuture<Result<ProxiedImage, ProxyError>>;
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

    /// Every account, by id, without secrets.
    fn accounts_full(&self) -> impl Future<Output = StoreResult<Vec<AccountDto>>> + Send;

    /// The stored account with its decrypted password, e.g. to test a
    /// patch merged over it; `None` if it does not exist.
    fn account_settings(
        &self,
        id: AccountId,
    ) -> impl Future<Output = StoreResult<Option<NewAccount>>> + Send;

    /// Applies `patch`, re-encrypting a new password and moving the default
    /// identity's address along with `email`; `None` if the account does
    /// not exist or is being deleted.
    fn update_account(
        &self,
        id: AccountId,
        patch: AccountPatch,
    ) -> impl Future<Output = StoreResult<Option<AccountDto>>> + Send;

    /// Sets `status = 'deleting'` and discards the stored password in one
    /// step, so only the first of concurrent deletes starts a removal.
    fn mark_deleting(
        &self,
        id: AccountId,
    ) -> impl Future<Output = StoreResult<MarkDeleting>> + Send;

    /// Persists the account with its password encrypted; returns its id.
    fn create_account(
        &self,
        account: NewAccount,
    ) -> impl Future<Output = StoreResult<AccountId>> + Send;

    /// A live (not tombstoned) message.
    fn message(
        &self,
        id: MessageRowId,
    ) -> impl Future<Output = StoreResult<Option<MessageRecord>>> + Send;

    /// A thread, following merges to the surviving thread; `None` if it
    /// does not exist or has no live messages.
    fn thread_detail(
        &self,
        id: ThreadId,
    ) -> impl Future<Output = StoreResult<Option<ThreadDetail>>> + Send;

    /// The cached RFC 822 message; `None` if it was never fetched or has
    /// been evicted.
    fn raw_message(
        &self,
        id: MessageRowId,
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send;

    /// Keys for signed part and proxy URLs, created on first use and
    /// rotated by the store when due.
    fn url_keys(&self) -> impl Future<Output = StoreResult<UrlKeys>> + Send;

    /// Lowercased addresses whose remote images always load.
    fn image_senders(&self) -> impl Future<Output = StoreResult<Vec<String>>> + Send;

    /// Adds or removes a lowercased address.
    fn set_image_sender(
        &self,
        address: String,
        allow: bool,
    ) -> impl Future<Output = StoreResult<()>> + Send;
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

    /// Fetches a message's raw body from the server into the store's raw
    /// cache, so that [`Store::raw_message`] returns it.
    fn fetch_raw(
        &self,
        account: AccountId,
        message: MessageRowId,
    ) -> BoxFuture<Result<(), FetchRawError>>;

    /// Called after a patch is committed. `restart`: connection settings,
    /// the password, `maxConnections` or the status changed, so the engine
    /// stops the account and starts it again if it is `active`.
    fn account_updated(&self, id: AccountId, restart: bool) {
        let _ = (id, restart);
    }

    /// Called after the account is marked `deleting`: stops it, purges its
    /// cache and config rows and emits the change events. Resolves when
    /// the purge is done.
    fn account_removed(&self, id: AccountId) -> BoxFuture<()> {
        let _ = id;
        Box::pin(async {})
    }

    /// Logs in to IMAP and SMTP with `settings` without saving anything.
    fn test_account(&self, settings: NewAccount) -> BoxFuture<AccountTestDto> {
        let _ = settings;
        let step = || TestStepDto {
            ok: false,
            error: Some("the engine cannot test accounts".to_owned()),
            ms: 0,
        };
        Box::pin(async move {
            AccountTestDto {
                imap: step(),
                smtp: step(),
                capabilities: Vec::new(),
                mailboxes: None,
            }
        })
    }
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
        assert_eq!(
            "account:2/inbox".parse(),
            Ok(View::Account(AccountId(2), Box::new(View::Inbox)))
        );
        assert_eq!(
            "account:2/label:Work".parse(),
            Ok(View::Account(
                AccountId(2),
                Box::new(View::Label(LabelKey::of("work")))
            ))
        );
        assert_eq!(
            "account:2/search:a/b:c".parse(),
            Ok(View::Account(
                AccountId(2),
                Box::new(View::Search(query::parse("a/b:c").unwrap()))
            ))
        );
        for bad in ["account:2/mailbox:3", "account:2/account:3/inbox"] {
            assert_eq!(bad.parse::<View>(), Err(ViewError::Scope), "{bad}");
        }
        for bad in [
            "",
            "Inbox",
            "account:",
            "account:2",
            "account:/inbox",
            "account:x/inbox",
            "account:2/",
            "account:2/x",
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
