//! Invitations (appointments plan §4 M-C steps 2–4): the RSVP, import and
//! apply requests of the API, and the `cal_inbox` rows mail sync records,
//! which the calendar runner turns into automatic applies.

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::Weak;

use tagmail_api_core::calendar_dto::{AutoApplyDto, RsvpPartstatDto};
use tagmail_api_core::compose::{ComposeError, ComposeResult};
use tagmail_api_core::itip_auth::{self, SENDER_FIELDS};
use tagmail_api_core::mime::reduce::{CALENDAR_PART_CAP, CALENDAR_PARTS_MAX, CalendarParts};
use tagmail_api_core::{BoxFuture, CalAction, CalRequest, Submitted, WriteError, WriteResult};
use tagmail_calendar::{Calendar, Event, Method};
use tagmail_core::{AccountId, EmailId};
use tagmail_imap::{FetchSpec, ImapSession, SelectOptions, Uid, UidSet};
use tagmail_store::calendar::{self, CaldavConfig};
use tagmail_store::rusqlite::{Connection, OptionalExtension, params};
use tagmail_store::{BlobKey, DEFAULT_RAW_CAP};
use time::OffsetDateTime;

use super::executor::{self, CalOp, CalOpKind};
use crate::calendar_hook::INVITE_MAX_AGE_SECS;
use crate::compose;
use crate::executor::now_secs;
use crate::identities::identities;
use crate::jobs::Priority;
use crate::supervisor::{ConnectorFactory, Engine, Inner, Stores};

/// `cal_inbox` rows handled per poll.
const INBOX_BATCH: usize = 50;
/// Transfer-encoded bytes fetched of a calendar part: base64 of a part at
/// [`CALENDAR_PART_CAP`] with its line breaks. A longer one is too large.
const ENCODED_PART_CAP: u64 = (CALENDAR_PART_CAP as u64) * 3 / 2;
/// How long reading an invitation or header sample over IMAP may take
/// before the row waits for the next poll.
const MAIL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Parse failures after which a `cal_inbox` row is dropped.
const MAX_PARSE_ATTEMPTS: u32 = 5;
/// INBOX messages whose `Authentication-Results` seed the suggestion.
const SUGGESTION_SAMPLE: u32 = itip_auth::SUGGESTION_WINDOW as u32;
const INBOX: &str = "INBOX";
const BOUNDARY: &str = "tagmail-calendar-parts";

/// What the calendar runner needs of mail: invitation parts of messages
/// that may not be cached, and recent sender headers.
pub(crate) trait MailAccess: Send + Sync {
    /// The calendar parts and sender fields of message `email_id`, fetched
    /// over IMAP (only those sections) when the raw message is not cached.
    /// `None` when the message is gone.
    fn invitation(
        &self,
        account: AccountId,
        email_id: String,
    ) -> BoxFuture<crate::Result<Option<CalendarParts>>>;

    /// `(topmost authserv-id, sender domain)` of the newest INBOX messages.
    fn authserv_samples(
        &self,
        account: AccountId,
    ) -> BoxFuture<crate::Result<Vec<(String, String)>>>;

    /// A REPLY was queued in the outbox.
    fn outbox_queued(&self);
}

/// The engine, without keeping it alive from its own runner.
pub(crate) struct WeakEngine<F: ConnectorFactory>(pub Weak<Inner<F>>);

impl<F: ConnectorFactory> WeakEngine<F> {
    fn engine(&self) -> crate::Result<Engine<F>> {
        self.0
            .upgrade()
            .map(|inner| Engine { inner })
            .ok_or_else(|| crate::Error::Unavailable("the engine stopped".to_owned()))
    }
}

impl<F: ConnectorFactory> MailAccess for WeakEngine<F> {
    fn invitation(
        &self,
        account: AccountId,
        email_id: String,
    ) -> BoxFuture<crate::Result<Option<CalendarParts>>> {
        let engine = self.engine();
        Box::pin(async move { engine?.invitation_parts(account, email_id).await })
    }

    fn authserv_samples(
        &self,
        account: AccountId,
    ) -> BoxFuture<crate::Result<Vec<(String, String)>>> {
        let engine = self.engine();
        Box::pin(async move { engine?.authserv_samples(account).await })
    }

    fn outbox_queued(&self) {
        if let Ok(engine) = self.engine() {
            engine.inner.outbox_wake.notify_one();
        }
    }
}

fn is_calendar(part: &tagmail_imap::PartInfo) -> bool {
    let name = part.filename.as_deref().map(crate::message::decode_text);
    tagmail_api_core::mime::is_calendar(&part.mime_type, name.as_deref().map(str::as_bytes))
}

/// A MIME message of `headers` and the fetched calendar leaves, for the
/// bounded extractor.
fn assemble(headers: &[u8], leaves: &[(&tagmail_imap::PartInfo, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(headers.trim_ascii_end());
    out.extend_from_slice(
        format!(
            "\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"{BOUNDARY}\"\r\n\r\n"
        )
        .as_bytes(),
    );
    for (part, body) in leaves {
        let charset = part
            .charset
            .as_deref()
            .map(|c| format!("; charset=\"{c}\""))
            .unwrap_or_default();
        out.extend_from_slice(
            format!(
                "--{BOUNDARY}\r\nContent-Type: {}{charset}\r\nContent-Transfer-Encoding: {}\r\n\r\n",
                part.mime_type, part.encoding
            )
            .as_bytes(),
        );
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

/// Fetches the sender fields and calendar leaves of `uid` in the selected
/// mailbox.
/// Fetches the sender fields and calendar leaves of `uid` in the selected
/// mailbox; also which leaves were cut at [`ENCODED_PART_CAP`].
async fn fetch_parts<S: ImapSession>(
    s: &mut S,
    uid: u32,
) -> crate::Result<Option<(Vec<u8>, Vec<bool>)>> {
    let spec = FetchSpec {
        body_structure: true,
        header_fields: SENDER_FIELDS.iter().map(|f| (*f).to_owned()).collect(),
        ..FetchSpec::default()
    };
    let mut uids = UidSet::new();
    uids.insert(Uid(uid));
    let metas = s.fetch(&uids, &spec).await?;
    let Some(meta) = metas.into_iter().find(|m| m.uid == Some(Uid(uid))) else {
        return Ok(None);
    };
    let headers = meta.header_fields.unwrap_or_default();
    let parts = meta.body.map(|b| b.parts).unwrap_or_default();
    let mut leaves = Vec::new();
    let mut cut = Vec::new();
    for part in parts
        .iter()
        .filter(|p| is_calendar(p))
        .take(CALENDAR_PARTS_MAX)
    {
        let mut body = Vec::new();
        let fetched = s
            .fetch_body(Uid(uid), &part.section, &mut body, ENCODED_PART_CAP)
            .await?;
        cut.push(fetched.truncated);
        if fetched.truncated {
            body.clear();
        }
        leaves.push((part, body));
    }
    Ok(Some((assemble(&headers, &leaves), cut)))
}

/// Calendar addresses of `account`'s identities, with their ids.
fn identity_addresses(conn: &Connection, account: AccountId) -> ComposeResult<Vec<(i64, String)>> {
    Ok(identities(conn)?
        .into_iter()
        .filter(|i| i.account_id == account.0)
        .map(|i| (i.id, i.address))
        .collect())
}

fn invalid(field: &str, message: impl Into<String>) -> WriteError {
    WriteError::invalid(field, message)
}

/// The parsed calendar parts of a message, with each part's text.
fn parsed(source: &CalendarParts, part: Option<u32>) -> Vec<(String, Calendar)> {
    source
        .parts
        .iter()
        .filter(|p| !p.too_large && part.is_none_or(|id| p.id == id))
        .filter_map(|p| {
            tagmail_calendar::parse(p.text.as_bytes())
                .ok()
                .map(|c| (p.text.clone(), c))
        })
        .collect()
}

/// The newest master (else first) VEVENT of the part's first UID.
fn main_event(calendar: &Calendar) -> Option<&Event> {
    let uid = &calendar.events.first()?.uid;
    calendar
        .events
        .iter()
        .filter(|e| &e.uid == uid)
        .max_by_key(|e| (e.recurrence_id.is_none(), e.sequence, e.dtstamp))
}

/// Refuses an update whose ORGANIZER differs from the stored event's
/// unless the owner allowed it.
async fn organizer_kept(
    stores: &Stores,
    target: AccountId,
    calendar: &Calendar,
    uid: &str,
    allowed: bool,
) -> WriteResult<()> {
    let owned = uid.to_owned();
    let stored = stores
        .cache
        .read(move |c| calendar::events_by_uid(c, &[target], &owned))
        .await
        .map_err(WriteError::internal)?;
    let events: Vec<&Event> = calendar.events.iter().filter(|e| e.uid == uid).collect();
    let changed = stored.first().is_some_and(|s| {
        !itip_auth::same_organizer(
            s.event.organizer.as_deref(),
            itip_auth::uid_organizer(&events),
        )
    });
    if changed && !allowed {
        return Err(invalid(
            "allowOrganizerChange",
            "the update names another organizer than the calendar event",
        ));
    }
    Ok(())
}

fn partstat_text(p: RsvpPartstatDto) -> &'static str {
    match p {
        RsvpPartstatDto::Accepted => "ACCEPTED",
        RsvpPartstatDto::Tentative => "TENTATIVE",
        RsvpPartstatDto::Declined => "DECLINED",
        RsvpPartstatDto::NeedsAction => "NEEDS-ACTION",
    }
}

/// Stages `ics` for an op; [`adopt`] moves it under the op's name once
/// the op is recorded. Blocking.
fn stage(dir: &Path, ics: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut file, ics.as_bytes())?;
    file.as_file().sync_all()?;
    file.keep().map(|(_, path)| path).map_err(|e| e.error)
}

fn adopt(dir: &Path, staged: &Path, op: &CalOp) -> std::io::Result<()> {
    match &op.ics_file {
        Some(name) => std::fs::rename(staged, dir.join(name)),
        None => Ok(()),
    }
}

fn store_err(e: impl Into<tagmail_store::Error>) -> ComposeError {
    ComposeError::Internal(Box::new(e.into()))
}

fn write_error(e: ComposeError) -> WriteError {
    match e {
        ComposeError::Invalid { field, message } => WriteError::invalid(field, message),
        ComposeError::NotFound(what) => WriteError::NotFound(format!("no such {what}")),
        ComposeError::Conflict(m) => WriteError::Conflict(m),
        other => WriteError::internal(other.to_string()),
    }
}

/// Records `op` with its object `ics`, and for an RSVP that sends one
/// now, its REPLY; `false` when the op id was used before.
async fn record(
    stores: &Stores,
    mut op: CalOp,
    ics: &str,
    reply: Option<Calendar>,
) -> ComposeResult<bool> {
    let dir = executor::ics_dir(stores);
    let keeps_object = op.state != tagmail_core::ops::OpState::Done;
    let staged = if keeps_object {
        let (dir, ics) = (dir.clone(), ics.to_owned());
        let staged = tokio::task::spawn_blocking(move || stage(&dir, &ics))
            .await
            .map_err(|e| ComposeError::Internal(Box::new(e)))?
            .map_err(store_err)?;
        op.ics_file = Some(format!("{}.ics", op.op_id));
        Some(staged)
    } else {
        None
    };
    let reply = match &reply {
        Some(invite) => match executor::write_reply(stores, &op, invite).await {
            Ok(reply) => Some(reply),
            Err(e) => {
                if let Some(staged) = &staged {
                    let _ = std::fs::remove_file(staged);
                }
                return Err(e);
            }
        },
        None => None,
    };
    let outbox = stores.outbox.clone();
    let (staged_path, queued) = (staged.clone(), reply.clone());
    let fresh = compose::write(stores, move |tx| {
        if !executor::insert(tx, &op).map_err(|e| store_err(tagmail_store::Error::from(e)))? {
            return Ok(false);
        }
        if let Some(reply) = &queued {
            executor::queue_reply(tx, &outbox, &op, reply)?;
        }
        if let Some(staged) = &staged_path {
            adopt(&dir, staged, &op).map_err(store_err)?;
        }
        Ok(true)
    })
    .await;
    if !matches!(fresh, Ok(true)) {
        if let Some(staged) = &staged {
            let _ = std::fs::remove_file(staged);
        }
        if let Some(reply) = &reply {
            reply.discard();
        }
    }
    fresh
}

impl<F: ConnectorFactory> Engine<F> {
    /// The id's op if it was used before: its status, or a conflict.
    pub(crate) async fn existing_cal(&self, op_id: &str) -> WriteResult<Option<Submitted>> {
        let id = op_id.to_owned();
        let seen = self
            .inner
            .stores
            .config
            .read(move |c| Ok(executor::seen_kind(c, &id)?))
            .await
            .map_err(WriteError::internal)?;
        match seen.as_deref() {
            None => Ok(None),
            Some(executor::CAL_OP) => match self.cal_op_dto(op_id.to_owned()).await? {
                Some(dto) => Ok(Some(Submitted::Existing(dto))),
                None => Err(WriteError::Conflict(
                    "this op id was used by an op that has been discarded".to_owned(),
                )),
            },
            Some(_) => Err(WriteError::Conflict(
                "this op id was used by a mail op".to_owned(),
            )),
        }
    }

    /// `POST /api/calendar/{rsvp,import,apply}`.
    pub(crate) async fn submit_calendar(&self, req: CalRequest) -> WriteResult<Submitted> {
        if let Some(existing) = self.existing_cal(&req.op_id).await? {
            return Ok(existing);
        }
        let stores = self.inner.stores.clone();
        let account = req.account;
        let addresses = stores
            .config
            .read(move |c| Ok(identity_addresses(c, account)))
            .await
            .map_err(WriteError::internal)?
            .map_err(write_error)?;
        let mapped = resolve_target(&stores, account).await?;
        let (op, ics, reply) = match req.action {
            CalAction::Rsvp {
                partstat,
                identity_id,
                allow_organizer_change,
                send_reply,
            } => {
                if send_reply && partstat == RsvpPartstatDto::NeedsAction {
                    return Err(invalid("partstat", "accepted, tentative or declined"));
                }
                if !send_reply && mapped.is_none() {
                    return Err(invalid("messageId", "no_calendar"));
                }
                let parts = parsed(&req.source, None);
                let Some((text, invite, event)) = parts.iter().find_map(|(text, c)| {
                    (c.method == Some(Method::Request))
                        .then(|| main_event(c))
                        .flatten()
                        .map(|e| (text.clone(), c.clone(), e.clone()))
                }) else {
                    return Err(invalid(
                        "messageId",
                        "the message has no invitation to answer",
                    ));
                };
                let me = addresses
                    .iter()
                    .filter(|(id, _)| identity_id.is_none_or(|want| want == *id))
                    .find(|(_, address)| event.attendees.iter().any(|a| a.is_address(address)));
                let Some((identity, me)) = me.cloned() else {
                    return Err(invalid(
                        "identityId",
                        "none of the account's identities is an attendee",
                    ));
                };
                if let Some((target, _, _)) = &mapped {
                    organizer_kept(
                        &stores,
                        *target,
                        &invite,
                        &event.uid,
                        allow_organizer_change,
                    )
                    .await?;
                }
                let mut op = CalOp::new(req.op_id, account, CalOpKind::Rsvp, event.uid.clone());
                op.partstat = Some(partstat_text(partstat).to_owned());
                op.attendee = Some(me);
                op.identity_id = Some(identity);
                op.reply_cancelled = !send_reply;
                let reply = match &mapped {
                    None => {
                        op.state = tagmail_core::ops::OpState::Done;
                        Some(invite)
                    }
                    Some((target, href, config)) => {
                        op.target = *target;
                        op.calendar_href = Some(href.clone());
                        let server = config.scheduling() == Some(calendar::Scheduling::Server);
                        (!server && send_reply).then_some(invite)
                    }
                };
                (op, text, reply)
            }
            CalAction::Import { part, calendar } => {
                let parts = parsed(&req.source, part);
                let Some((text, cal)) = parts.into_iter().next() else {
                    return Err(invalid("partId", "no readable calendar part"));
                };
                let Some(uid) = cal.events.first().map(|e| e.uid.clone()) else {
                    return Err(invalid("partId", "the calendar part has no events"));
                };
                let (target, href) = match calendar {
                    Some((target, href)) => {
                        let known = stores
                            .cache
                            .read({
                                let href = href.clone();
                                move |c| {
                                    Ok(c.prepare_cached(
                                        "SELECT 1 FROM calendar WHERE account_id = ?1 AND href = ?2",
                                    )?
                                    .exists(params![target.0, href])?)
                                }
                            })
                            .await
                            .map_err(WriteError::internal)?;
                        if !known {
                            return Err(invalid("calendarHref", "no such calendar"));
                        }
                        (target, href)
                    }
                    None => match &mapped {
                        Some((target, href, _)) => (*target, href.clone()),
                        None => return Err(invalid("calendarHref", "no_calendar")),
                    },
                };
                let mut op = CalOp::new(req.op_id, account, CalOpKind::Import, uid);
                op.target = target;
                op.calendar_href = Some(href);
                (op, text, None)
            }
            CalAction::Apply {
                allow_organizer_change,
            } => {
                let parts = parsed(&req.source, None);
                let Some((text, cal, event)) = parts.iter().find_map(|(text, c)| {
                    matches!(c.method, Some(Method::Request | Method::Cancel))
                        .then(|| main_event(c))
                        .flatten()
                        .map(|e| (text.clone(), c, e.clone()))
                }) else {
                    return Err(invalid("messageId", "the message has no update to apply"));
                };
                let Some((target, href, _)) = &mapped else {
                    return Err(invalid("messageId", "no_calendar"));
                };
                organizer_kept(&stores, *target, cal, &event.uid, allow_organizer_change).await?;
                let me = addresses
                    .iter()
                    .find(|(_, a)| event.attendees.iter().any(|x| x.is_address(a)))
                    .map(|(_, a)| a.clone());
                let mut op = CalOp::new(req.op_id, account, CalOpKind::Apply, event.uid.clone());
                op.target = *target;
                op.calendar_href = Some(href.clone());
                op.attendee = me;
                (op, text, None)
            }
        };
        let op_id = op.op_id.clone();
        let replies = reply.is_some();
        let fresh = record(&stores, op, &ics, reply)
            .await
            .map_err(write_error)?;
        if !fresh {
            return self
                .existing_cal(&op_id)
                .await?
                .ok_or_else(|| WriteError::Conflict("the op id was just used".to_owned()));
        }
        executor::emit(&stores, &op_id).await;
        if replies {
            self.inner.outbox_wake.notify_one();
        }
        self.wake_calendar_ops();
        let dto = self
            .cal_op_dto(op_id)
            .await?
            .ok_or_else(|| WriteError::NotFound("no such op".to_owned()))?;
        Ok(Submitted::Created(dto))
    }

    /// See [`MailAccess::invitation`].
    async fn invitation_parts(
        &self,
        account: AccountId,
        email_id: String,
    ) -> crate::Result<Option<CalendarParts>> {
        let ctx = self.ctx(account);
        let id = email_id.clone();
        let locations: Vec<(String, u32)> = ctx
            .read(move |c| {
                let mut stmt = c.prepare_cached(
                    "SELECT mb.name, l.uid FROM message m \
                     JOIN location l ON l.message_id = m.id \
                     JOIN mailbox mb ON mb.id = l.mailbox_id \
                     WHERE m.account_id = ?1 AND m.email_id = ?2 AND m.tombstoned_at IS NULL \
                     ORDER BY (mb.name = 'INBOX' COLLATE NOCASE) DESC, mb.id",
                )?;
                let rows = stmt
                    .query_map(params![account.0, id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<tagmail_store::rusqlite::Result<_>>()?;
                Ok(rows)
            })
            .await?;
        if locations.is_empty() {
            return Ok(None);
        }
        if let Ok(parsed_id) = EmailId::parse(&email_id) {
            let key = BlobKey::for_message(account, &parsed_id);
            let blobs = self.inner.stores.blobs.clone();
            let cached = tokio::task::spawn_blocking(move || {
                let Some(reader) = blobs.open_raw(&key)? else {
                    return Ok::<_, tagmail_store::Error>(None);
                };
                Ok(Some(tagmail_api_core::mime::reduce::calendar_parts(
                    BufReader::new(reader.take(DEFAULT_RAW_CAP)),
                )?))
            })
            .await
            .map_err(|_| tagmail_store::Error::Closed)??;
            if let Some(parts) = cached {
                return Ok(Some(parts));
            }
        }
        let raw = self
            .run_job(account, Priority::Background, move |s, _| {
                Box::pin(async move {
                    for (mailbox, uid) in locations {
                        if s.selected() != Some(mailbox.as_str()) {
                            match s.select(&mailbox, SelectOptions::default()).await {
                                Ok(_) => {}
                                Err(e) if e.is_fatal() => return Err(e.into()),
                                Err(_) => continue,
                            }
                        }
                        if let Some(raw) = fetch_parts(s, uid).await? {
                            return Ok(Some(raw));
                        }
                    }
                    Ok(None)
                })
            })
            .await?;
        let Some((raw, cut)) = raw else {
            return Ok(None);
        };
        let mut parts = tagmail_api_core::mime::reduce::calendar_parts(raw.as_slice())
            .map_err(tagmail_store::Error::from)?;
        for (part, cut) in parts.parts.iter_mut().zip(cut) {
            if cut {
                part.too_large = true;
                part.text.clear();
            }
        }
        Ok(Some(parts))
    }

    /// See [`MailAccess::authserv_samples`].
    async fn authserv_samples(&self, account: AccountId) -> crate::Result<Vec<(String, String)>> {
        self.run_job(account, Priority::Background, move |s, _| {
            Box::pin(async move {
                let selected = s.select(INBOX, SelectOptions::default()).await?;
                let next = selected.uid_next.map_or(0, |u| u.0);
                if next <= 1 {
                    return Ok(Vec::new());
                }
                let first = next.saturating_sub(SUGGESTION_SAMPLE).max(1);
                let spec = FetchSpec {
                    header_fields: SENDER_FIELDS.iter().map(|f| (*f).to_owned()).collect(),
                    ..FetchSpec::default()
                };
                let mut uids = UidSet::new();
                uids.insert_range(first, next - 1);
                let metas = s.fetch(&uids, &spec).await?;
                let mut samples: Vec<(u32, String, String)> = metas
                    .into_iter()
                    .filter_map(|m| {
                        let headers = m.header_fields?;
                        Some((
                            m.uid?.0,
                            itip_auth::topmost_authserv_id(&headers)?,
                            itip_auth::sender_domain(&headers)?,
                        ))
                    })
                    .collect();
                samples.sort_by_key(|(uid, ..)| std::cmp::Reverse(*uid));
                Ok(samples.into_iter().map(|(_, id, d)| (id, d)).collect())
            })
        })
        .await
    }
}

async fn resolve_target(
    stores: &Stores,
    account: AccountId,
) -> WriteResult<Option<(AccountId, String, CaldavConfig)>> {
    let configs = stores
        .config
        .read(move |c| {
            let own = calendar::load_caldav(c, account)?.unwrap_or_default();
            let fallback = match own.fallback_account {
                Some(f) => Some((f, calendar::load_caldav(c, f)?.unwrap_or_default())),
                None => None,
            };
            Ok((own, fallback))
        })
        .await
        .map_err(WriteError::internal)?;
    let (own, fallback) = configs;
    let mut mapped = vec![(account, own)];
    mapped.extend(fallback);
    let candidates: Vec<(AccountId, String)> = mapped
        .iter()
        .filter_map(|(a, c)| c.default_calendar_href.clone().map(|h| (*a, h)))
        .collect();
    let known = stores
        .cache
        .read(move |c| {
            let mut stmt =
                c.prepare_cached("SELECT 1 FROM calendar WHERE account_id = ?1 AND href = ?2")?;
            for (a, h) in candidates {
                if stmt.exists(params![a.0, h])? {
                    return Ok(Some((a, h)));
                }
            }
            Ok(None)
        })
        .await
        .map_err(WriteError::internal)?;
    Ok(known.and_then(|(a, h)| {
        mapped
            .into_iter()
            .find(|(id, _)| *id == a)
            .map(|(_, c)| (a, h, c))
    }))
}

/// What became of one `cal_inbox` row.
enum Row {
    /// Processed or not applicable: the row goes.
    Done,
    /// Processed and an apply was queued: the row goes.
    Applied,
    /// Not yet: the target has not synced, or the message could not be
    /// read for a transient reason.
    Wait,
    /// No calendar part parses.
    Unparsable,
}

/// Queues the deterministic-id apply of `event` (plan §3.6); a known id is
/// a no-op. Returns whether the op is new.
async fn queue_apply(
    stores: &Stores,
    account: AccountId,
    target: (AccountId, String),
    method: &Method,
    event: &Event,
    text: &str,
    me: Option<String>,
) -> crate::Result<bool> {
    let recurrence = event.recurrence_id.as_ref().map(itip_auth::recurrence_key);
    let id = itip_auth::apply_op_id(
        account.0,
        &event.uid,
        recurrence.as_deref(),
        event.sequence,
        event.dtstamp.map(OffsetDateTime::unix_timestamp),
        method.as_str(),
        None,
    );
    let mut op = CalOp::new(id.clone(), account, CalOpKind::Apply, event.uid.clone());
    op.target = target.0;
    op.calendar_href = Some(target.1);
    op.attendee = me;
    let fresh = record(stores, op, text, None)
        .await
        .map_err(|e| crate::Error::Unavailable(e.to_string()))?;
    if fresh {
        tracing::info!(op = %id, uid = %event.uid, "invitation update queued for apply");
        executor::emit(stores, &id).await;
    }
    Ok(fresh)
}

async fn inbox_row(
    stores: &Stores,
    mail: &dyn MailAccess,
    account: AccountId,
    email_id: String,
    received: i64,
    now: i64,
) -> Row {
    let mapped = match resolve_target(stores, account).await {
        Ok(Some(mapped)) => mapped,
        Ok(None) => return Row::Done,
        Err(e) => {
            tracing::warn!(account = account.0, error = %e, "cannot resolve the calendar target");
            return Row::Wait;
        }
    };
    let (target, href, _) = mapped;
    let synced = {
        let href = href.clone();
        stores
            .cache
            .read(move |c| {
                Ok(c.query_row(
                    "SELECT synced_at FROM calendar WHERE account_id = ?1 AND href = ?2",
                    params![target.0, href],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?
                .flatten())
            })
            .await
    };
    if !matches!(synced, Ok(Some(_))) {
        return Row::Wait;
    }
    let fetched = tokio::time::timeout(MAIL_TIMEOUT, mail.invitation(account, email_id.clone()))
        .await
        .unwrap_or_else(|_| Err(crate::Error::Unavailable("timed out".to_owned())));
    let source = match fetched {
        Ok(Some(source)) => source,
        Ok(None) => return Row::Done,
        Err(e) => {
            tracing::debug!(account = account.0, email = %email_id, error = %e, "invitation not readable yet");
            return Row::Wait;
        }
    };
    let calendars = parsed(&source, None);
    if calendars.is_empty() {
        return if source.parts.iter().all(|p| p.too_large) {
            Row::Done
        } else {
            Row::Unparsable
        };
    }
    let context = stores
        .config
        .read(move |c| {
            let trusted = calendar::load_caldav(c, account)?.and_then(|c| c.authserv_id);
            Ok((trusted, identity_addresses(c, account)))
        })
        .await;
    let (trusted, addresses) = match context {
        Ok((trusted, Ok(addresses))) => (trusted, addresses),
        _ => return Row::Wait,
    };
    let mut applied = false;
    for (text, cal) in &calendars {
        let Some(method) = cal
            .method
            .as_ref()
            .filter(|m| matches!(m, Method::Request | Method::Cancel))
        else {
            continue;
        };
        let mut uids: Vec<&str> = cal.events.iter().map(|e| e.uid.as_str()).collect();
        uids.dedup();
        for uid in uids {
            let of_uid: Vec<&Event> = cal.events.iter().filter(|e| e.uid == uid).collect();
            let Some(event) = of_uid
                .iter()
                .copied()
                .max_by_key(|e| (e.recurrence_id.is_none(), e.sequence, e.dtstamp))
            else {
                continue;
            };
            let owned = uid.to_owned();
            let stored = stores
                .cache
                .read(move |c| calendar::events_by_uid(c, &[target], &owned))
                .await;
            let Ok(stored) = stored else {
                return Row::Wait;
            };
            let held = stored.first().map(|s| itip_auth::StoredVersion {
                sequence: s.event.sequence,
                dtstamp: s.event.dtstamp,
                cancelled: s
                    .event
                    .status
                    .as_deref()
                    .is_some_and(|st| st.eq_ignore_ascii_case("CANCELLED")),
                partstat: s.event.my_partstat.clone(),
                organizer: s.event.organizer.clone(),
            });
            let sender = itip_auth::sender_auth(
                [(Some(method), event)],
                &source.headers,
                trusted.as_deref(),
            );
            let decision = itip_auth::decide(&itip_auth::UpdateCheck {
                method: Some(method),
                incoming: itip_auth::IncomingVersion {
                    sequence: event.sequence,
                    dtstamp: event.dtstamp.map(OffsetDateTime::unix_timestamp),
                },
                organizer: itip_auth::uid_organizer(&of_uid),
                overrides_only: of_uid.iter().all(|e| e.recurrence_id.is_some()),
                past: itip_auth::past(&of_uid, now),
                stored: held.as_ref(),
                sender,
                received: Some(received),
                now,
                has_target: true,
            });
            if decision.auto_apply != AutoApplyDto::Pending {
                continue;
            }
            let me = addresses
                .iter()
                .find(|(_, a)| event.attendees.iter().any(|x| x.is_address(a)))
                .map(|(_, a)| a.clone());
            let queued = queue_apply(
                stores,
                account,
                (target, href.clone()),
                method,
                event,
                text,
                me,
            )
            .await;
            match queued {
                Ok(fresh) => applied |= fresh,
                Err(e) => {
                    tracing::warn!(account = account.0, error = %e, "cannot queue an invitation update");
                    return Row::Wait;
                }
            }
        }
    }
    if applied { Row::Applied } else { Row::Done }
}

/// What a pass over `cal_inbox` did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct InboxPass {
    /// An apply was queued.
    pub queued: bool,
    /// A full batch was read: more rows may be due.
    pub more: bool,
}

/// Processes the due `cal_inbox` rows (plan §3.1), taking each account in
/// turn so one busy account does not starve the others. A row that cannot
/// be read yet backs off per wait.
pub(crate) async fn process_inbox(stores: &Stores, mail: &dyn MailAccess) -> InboxPass {
    let now = now_secs();
    let rows = stores
        .cache
        .read(move |c| {
            let mut stmt = c.prepare_cached(
                "SELECT account_id, email_id, internal_date, attempts, waits FROM \
                 (SELECT *, row_number() OVER \
                    (PARTITION BY account_id ORDER BY internal_date) AS turn \
                  FROM cal_inbox WHERE next_at <= ?1) \
                 ORDER BY turn, internal_date LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![now, INBOX_BATCH as i64], |r| {
                    Ok((
                        AccountId(r.get(0)?),
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, u32>(3)?,
                        r.get::<_, u32>(4)?,
                    ))
                })?
                .collect::<tagmail_store::rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the invitation queue");
            return InboxPass::default();
        }
    };
    let mut pass = InboxPass {
        queued: false,
        more: rows.len() == INBOX_BATCH,
    };
    for (account, email_id, internal_date, attempts, waits) in rows {
        let row = if internal_date < now - INVITE_MAX_AGE_SECS {
            Row::Done
        } else {
            inbox_row(stores, mail, account, email_id.clone(), internal_date, now).await
        };
        pass.queued |= matches!(row, Row::Applied);
        let attempts = attempts + 1;
        let waits = waits + 1;
        let next_at = now.saturating_add(
            i64::try_from(crate::executor::backoff(waits).as_secs()).unwrap_or(i64::MAX),
        );
        let settled = stores
            .cache
            .write(move |tx| {
                match row {
                    Row::Done | Row::Applied => {
                        tx.execute(
                            "DELETE FROM cal_inbox WHERE account_id = ?1 AND email_id = ?2",
                            params![account.0, email_id],
                        )?;
                    }
                    Row::Unparsable if attempts >= MAX_PARSE_ATTEMPTS => {
                        tracing::warn!(account = account.0, email = %email_id, "invitation does not parse; dropped");
                        tx.execute(
                            "DELETE FROM cal_inbox WHERE account_id = ?1 AND email_id = ?2",
                            params![account.0, email_id],
                        )?;
                    }
                    Row::Unparsable => {
                        tx.execute(
                            "UPDATE cal_inbox SET attempts = ?3 \
                             WHERE account_id = ?1 AND email_id = ?2",
                            params![account.0, email_id, attempts],
                        )?;
                    }
                    Row::Wait => {
                        tx.execute(
                            "UPDATE cal_inbox SET waits = ?3, next_at = ?4 \
                             WHERE account_id = ?1 AND email_id = ?2",
                            params![account.0, email_id, waits, next_at],
                        )?;
                    }
                }
                Ok(())
            })
            .await;
        if let Err(e) = settled {
            tracing::error!(error = %e, "cannot update the invitation queue");
            pass.more = false;
        }
    }
    pass
}

/// Refreshes the authserv-id each CalDAV account is offered (plan §3.4).
pub(crate) async fn refresh_suggestions(stores: &Stores, mail: &dyn MailAccess) {
    let accounts = stores
        .config
        .read(|c| {
            let mut stmt =
                c.prepare("SELECT id FROM account WHERE status = 'active' ORDER BY id")?;
            Ok(stmt
                .query_map([], |r| r.get(0).map(AccountId))?
                .collect::<tagmail_store::rusqlite::Result<Vec<_>>>()?)
        })
        .await;
    let Ok(accounts) = accounts else {
        return;
    };
    for account in accounts {
        if !matches!(resolve_target(stores, account).await, Ok(Some(_))) {
            continue;
        }
        let samples = tokio::time::timeout(MAIL_TIMEOUT, mail.authserv_samples(account))
            .await
            .unwrap_or_else(|_| Err(crate::Error::Unavailable("timed out".to_owned())));
        let samples = match samples {
            Ok(samples) => samples,
            Err(e) => {
                tracing::debug!(account = account.0, error = %e, "no Authentication-Results sample");
                continue;
            }
        };
        let suggestion = itip_auth::suggestion(&samples);
        let saved = stores
            .config
            .write(move |tx| calendar::set_authserv_suggestion(tx, account, suggestion.as_deref()))
            .await;
        if let Err(e) = saved {
            tracing::warn!(account = account.0, error = %e, "cannot save the authserv-id suggestion");
        }
    }
}
