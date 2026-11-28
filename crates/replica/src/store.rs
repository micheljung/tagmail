//! The desktop's [`Store`] (§7): the replica's local store per request,
//! with the raw fetch, image proxy and offline text fallback the server
//! provides on the web.

use std::future::Future;
#[cfg(feature = "calendar")]
use std::ops::Range;
use std::sync::Arc;

use reqwest::Method;
use rusqlite::{OptionalExtension, params};
#[cfg(feature = "calendar")]
use tagmail_api_core::calendar_dto::{
    CalEventDto, CalendarDto, CalendarTargetDto, EventInstanceDto,
};
use tagmail_api_core::dto::{AccountDto, LabelDto, MailboxDto};
use tagmail_api_core::mime::reduce::CalendarParts;
use tagmail_api_core::replica_dto::ReplicaImageDto;
use tagmail_api_core::signing::UrlKeys;
use tagmail_api_core::sqlite_store::SqliteStore;
#[cfg(feature = "calendar")]
use tagmail_api_core::store::InvitationContext;
use tagmail_api_core::store::{
    AccountPatch, AccountRegistry, AccountSummary, BoxFuture, ChangePage, FetchRawError,
    ImageProxy, MarkDeleting, MessageRecord, NewAccount, ProxiedImage, ProxyError, RuntimeStatus,
    Store, StoreResult, ThreadDetail, ThreadRows, View, ViewIndex,
};
use tagmail_core::{AccountId, ChangeCursor, ChangeFilter, MessageRowId, ThreadId};
use tagmail_store::BlobKey;

use crate::apply;
use crate::client::{ClientError, REQUEST_TIMEOUT};
use crate::sync::Replica;
use crate::window;

pub const NOT_DOWNLOADED: &str = "Full message not downloaded.";

/// The replica's store as one request sees it; `online` is sampled when
/// the request starts.
pub struct ReplicaStore {
    inner: Arc<SqliteStore>,
    online: bool,
}

impl ReplicaStore {
    pub fn new(inner: Arc<SqliteStore>, online: bool) -> ReplicaStore {
        ReplicaStore { inner, online }
    }

    /// The message's synced text as a plain-text message, for an offline
    /// open of a message whose raw copy was never downloaded.
    async fn text_fallback(&self, id: MessageRowId) -> StoreResult<Option<Vec<u8>>> {
        let blobs = Arc::clone(self.inner.blobs());
        let text = self
            .inner
            .cache()
            .read(move |c| {
                let key: Option<String> = c
                    .query_row(
                        "SELECT text_blob FROM message WHERE id = ?1 AND tombstoned_at IS NULL",
                        [id.0],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                match key {
                    Some(key) => blobs.read_text(&BlobKey::parse(&key)?),
                    None => Ok(None),
                }
            })
            .await?;
        let Some(text) = text else {
            return Ok(None);
        };
        let message = format!(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n\
             {NOT_DOWNLOADED}\r\n\r\n{text}"
        );
        Ok(Some(tagmail_api_core::mime::reduce::reduce(
            message.as_bytes(),
        )?))
    }
}

impl Store for ReplicaStore {
    fn head(&self) -> impl Future<Output = StoreResult<ChangeCursor>> + Send {
        self.inner.head()
    }

    fn view_index(&self, view: View) -> impl Future<Output = StoreResult<ViewIndex>> + Send {
        self.inner.view_index(view)
    }

    fn mailboxes(&self) -> impl Future<Output = StoreResult<Vec<MailboxDto>>> + Send {
        self.inner.mailboxes()
    }

    fn labels(&self) -> impl Future<Output = StoreResult<Vec<LabelDto>>> + Send {
        self.inner.labels()
    }

    fn threads(&self, ids: Vec<ThreadId>) -> impl Future<Output = StoreResult<ThreadRows>> + Send {
        self.inner.threads(ids)
    }

    fn changes_since(
        &self,
        since: ChangeCursor,
        limit: usize,
        filter: ChangeFilter,
    ) -> impl Future<Output = StoreResult<ChangePage>> + Send {
        self.inner.changes_since(since, limit, filter)
    }

    fn accounts(&self) -> impl Future<Output = StoreResult<Vec<AccountSummary>>> + Send {
        self.inner.accounts()
    }

    fn accounts_full(&self) -> impl Future<Output = StoreResult<Vec<AccountDto>>> + Send {
        self.inner.accounts_full()
    }

    fn account_settings(
        &self,
        id: AccountId,
    ) -> impl Future<Output = StoreResult<Option<NewAccount>>> + Send {
        self.inner.account_settings(id)
    }

    fn update_account(
        &self,
        id: AccountId,
        patch: AccountPatch,
    ) -> impl Future<Output = StoreResult<Option<AccountDto>>> + Send {
        self.inner.update_account(id, patch)
    }

    fn mark_deleting(
        &self,
        id: AccountId,
    ) -> impl Future<Output = StoreResult<MarkDeleting>> + Send {
        self.inner.mark_deleting(id)
    }

    fn create_account(
        &self,
        account: NewAccount,
    ) -> impl Future<Output = StoreResult<AccountId>> + Send {
        self.inner.create_account(account)
    }

    fn message(
        &self,
        id: MessageRowId,
    ) -> impl Future<Output = StoreResult<Option<MessageRecord>>> + Send {
        self.inner.message(id)
    }

    fn thread_detail(
        &self,
        id: ThreadId,
    ) -> impl Future<Output = StoreResult<Option<ThreadDetail>>> + Send {
        self.inner.thread_detail(id)
    }

    fn raw_message(
        &self,
        id: MessageRowId,
    ) -> impl Future<Output = StoreResult<Option<Vec<u8>>>> + Send {
        self.inner.raw_message(id)
    }

    fn calendar_source(
        &self,
        id: MessageRowId,
    ) -> impl Future<Output = StoreResult<Option<CalendarParts>>> + Send {
        self.inner.calendar_source(id)
    }

    async fn render_source(&self, id: MessageRowId) -> StoreResult<Option<Vec<u8>>> {
        if let Some(source) = self.inner.render_source(id).await? {
            return Ok(Some(source));
        }
        if self.online {
            return Ok(None);
        }
        self.text_fallback(id).await
    }

    fn url_keys(&self) -> impl Future<Output = StoreResult<UrlKeys>> + Send {
        self.inner.url_keys()
    }

    fn image_senders(&self) -> impl Future<Output = StoreResult<Vec<String>>> + Send {
        self.inner.image_senders()
    }

    fn set_image_sender(
        &self,
        address: String,
        allow: bool,
    ) -> impl Future<Output = StoreResult<()>> + Send {
        self.inner.set_image_sender(address, allow)
    }

    #[cfg(feature = "calendar")]
    fn calendars(&self) -> impl Future<Output = StoreResult<Vec<CalendarDto>>> + Send {
        self.inner.calendars()
    }

    #[cfg(feature = "calendar")]
    fn events_in(
        &self,
        range: Range<i64>,
        calendars: Vec<CalendarTargetDto>,
    ) -> impl Future<Output = StoreResult<Vec<EventInstanceDto>>> + Send {
        self.inner.events_in(range, calendars)
    }

    #[cfg(feature = "calendar")]
    fn event(
        &self,
        account: AccountId,
        href: String,
    ) -> impl Future<Output = StoreResult<Option<CalEventDto>>> + Send {
        self.inner.event(account, href)
    }

    #[cfg(feature = "calendar")]
    fn event_by_uid(
        &self,
        account: AccountId,
        uid: String,
    ) -> impl Future<Output = StoreResult<Vec<CalEventDto>>> + Send {
        self.inner.event_by_uid(account, uid)
    }

    #[cfg(feature = "calendar")]
    fn invitation_context(
        &self,
        account: AccountId,
        message: MessageRowId,
        uids: Vec<String>,
    ) -> impl Future<Output = StoreResult<InvitationContext>> + Send {
        self.inner.invitation_context(account, message, uids)
    }
}

/// Downloads raw messages from the server into the replica on open.
pub struct DesktopRegistry(pub Arc<Replica>);

impl Replica {
    async fn fetch_raw_now(&self, message: i64) -> Result<(), FetchRawError> {
        let unavailable = |e: &dyn std::fmt::Display| FetchRawError::Unavailable(e.to_string());
        let store = self
            .store()
            .ok_or_else(|| unavailable(&crate::compose::NOT_READY))?;
        let key = store
            .cache()
            .read(move |c| {
                apply::blob_key(c, message)
                    .map_err(|e| tagmail_store::Error::Corrupt(e.to_string()))
            })
            .await
            .map_err(|e| unavailable(&e))?
            .ok_or(FetchRawError::NotFound)?;
        let key = match window::fetch_raw(self.client(), store.blobs(), message, key).await {
            Ok(key) => key,
            Err(crate::Error::Client(ClientError::Api { status: 404, .. })) => {
                return Err(FetchRawError::NotFound);
            }
            Err(e) => return Err(unavailable(&e)),
        };
        store
            .cache()
            .write(move |tx| {
                tx.execute(
                    "UPDATE message SET raw_blob = ?2 WHERE id = ?1",
                    params![message, key.as_str()],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| unavailable(&e))
    }

    async fn fetch_image(&self, url: String) -> Result<ProxiedImage, ProxyError> {
        let builder = self
            .client()
            .request(Method::POST, "api/replica/image")
            .map_err(|e| ProxyError::Upstream(e.to_string()))?
            .timeout(REQUEST_TIMEOUT)
            .json(&ReplicaImageDto { url });
        let resp = match self.client().send(builder).await {
            Ok(resp) => resp,
            Err(ClientError::Api { status: 403, body }) => {
                return Err(ProxyError::Blocked(body.message));
            }
            Err(ClientError::Api { status: 504, .. }) => return Err(ProxyError::Timeout),
            Err(ClientError::Api { body, .. }) if body.error == "too_large" => {
                return Err(ProxyError::TooLarge);
            }
            Err(e) => return Err(ProxyError::Upstream(e.to_string())),
        };
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(image_type)
            .ok_or_else(|| ProxyError::Upstream("not an image".into()))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ProxyError::Upstream(e.to_string()))?;
        Ok(ProxiedImage {
            content_type,
            bytes: bytes.to_vec(),
        })
    }
}

/// The server sniffs the type; only these are passed on.
fn image_type(value: &str) -> Option<&'static str> {
    const TYPES: &[&str] = &[
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        "image/avif",
        "image/bmp",
        "image/x-icon",
        "image/svg+xml",
    ];
    let essence = value.split(';').next()?.trim();
    TYPES
        .iter()
        .copied()
        .find(|t| t.eq_ignore_ascii_case(essence))
}

impl AccountRegistry for DesktopRegistry {
    fn account_added(&self, _: AccountId) {}

    fn runtime_status(&self, _: AccountId) -> Option<RuntimeStatus> {
        None
    }

    fn fetch_raw(
        &self,
        _: AccountId,
        message: MessageRowId,
    ) -> BoxFuture<Result<(), FetchRawError>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.fetch_raw_now(message.0).await })
    }
}

/// Remote images through the server's guarded fetcher.
pub struct DesktopImages(pub Arc<Replica>);

impl ImageProxy for DesktopImages {
    fn fetch(&self, url: String) -> BoxFuture<Result<ProxiedImage, ProxyError>> {
        let r = Arc::clone(&self.0);
        Box::pin(async move { r.fetch_image(url).await })
    }
}
