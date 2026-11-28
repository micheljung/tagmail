//! Events the owner creates, edits, deletes or moves (plan §4 M-F):
//! `POST /api/calendar/ops` becomes a `put`, `delete` or `move` op on the
//! cached copy. With attendees and client scheduling (§3.3) the account
//! that organizes the event sends the REQUEST or CANCEL through the
//! outbox, under `<opId>#request` and `<opId>#cancel`.

use tagmail_api_core::calendar_dto::{CalEditDto, CalEditKindDto};
use tagmail_api_core::compose::{ComposeError, ComposeResult};
use tagmail_api_core::compose_dto::IdentityDto;
use tagmail_api_core::dto::AddressDto;
use tagmail_api_core::{Submitted, WriteError, WriteResult, event_fields};
use tagmail_calendar::{EditOrganizer, Method};
use tagmail_core::AccountId;
use tagmail_store::calendar::{self, Scheduling};
use tagmail_store::rusqlite::{OptionalExtension, params};
use time::OffsetDateTime;

use super::executor::{self, CalOp, CalOpKind};
use super::invite::record;
use crate::compose::{self, GeneratedMessage};
use crate::identities::identities;
use crate::supervisor::{ConnectorFactory, Engine, Stores};

pub(crate) const REQUEST_SUFFIX: &str = "#request";
pub(crate) const CANCEL_SUFFIX: &str = "#cancel";
const UID_BYTES: usize = 16;
const UID_DOMAIN: &str = "tagmail";

/// The cached copy an edit starts from.
struct Stored {
    etag: Option<String>,
    uid: String,
    ics: Vec<u8>,
}

fn new_uid() -> String {
    let mut random = [0u8; UID_BYTES];
    getrandom::fill(&mut random).expect("the OS random source is available");
    let hex: String = random.iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}@{UID_DOMAIN}")
}

fn invalid(field: &str, message: impl Into<String>) -> WriteError {
    WriteError::invalid(field, message)
}

fn compose_error(e: ComposeError) -> WriteError {
    match e {
        ComposeError::Invalid { field, message } => WriteError::invalid(field, message),
        other => WriteError::internal(other.to_string()),
    }
}

/// An iTIP message of `method` from `identity` to `to`, as the outbox
/// sends it.
async fn itip(
    stores: &Stores,
    op_id: String,
    identity: &IdentityDto,
    to: Vec<AddressDto>,
    summary: Option<&str>,
    ics: String,
    method: Method,
) -> ComposeResult<GeneratedMessage> {
    let prefix = match method {
        Method::Cancel => "Cancelled",
        _ => "Invitation",
    };
    let subject = format!("{prefix}: {}", summary.unwrap_or("(no title)"));
    let build = |from: &AddressDto, msgid: &str, date: i64| {
        crate::outgoing::itip_mime(from, &to, &subject, msgid, date, &ics, &method)
    };
    compose::write_generated(
        stores,
        op_id,
        identity.account_id,
        Some(identity.id),
        &to,
        subject.clone(),
        &build,
    )
    .await
}

fn recipients(addresses: &[String], organizer: &str) -> Vec<AddressDto> {
    addresses
        .iter()
        .filter(|a| !a.eq_ignore_ascii_case(organizer))
        .map(|a| AddressDto {
            name: None,
            email: a.clone(),
        })
        .collect()
}

impl<F: ConnectorFactory> Engine<F> {
    /// `POST /api/calendar/ops`.
    pub(crate) async fn submit_edit(&self, dto: CalEditDto) -> WriteResult<Submitted> {
        if let Some(existing) = self.existing_cal(&dto.op_id).await? {
            return Ok(existing);
        }
        let stores = self.inner.stores.clone();
        let account = AccountId(dto.account_id);
        let (config, mine, addresses) = stores
            .config
            .read(move |c| {
                let config = calendar::load_caldav(c, account)?.unwrap_or_default();
                Ok((config, identities(c), calendar::my_addresses(c, account)?))
            })
            .await
            .map_err(WriteError::internal)?;
        let mine: Vec<IdentityDto> = mine
            .map_err(compose_error)?
            .into_iter()
            .filter(|i| i.account_id == account.0)
            .collect();
        let (calendar_href, href, dest) = (
            dto.calendar_href.clone(),
            dto.href.clone(),
            dto.dest_calendar_href.clone(),
        );
        let (known, dest_known, stored) = stores
            .cache
            .read(move |c| {
                let calendars = calendar::calendars(c, Some(account))?;
                let has = |h: &str| calendars.iter().any(|r| r.href == h && r.writable);
                let stored = match &href {
                    Some(href) => {
                        let row: Option<(i64, Option<String>, String)> = c
                            .query_row(
                                "SELECT e.id, e.etag, e.uid FROM cal_event e \
                                 JOIN calendar k ON k.id = e.calendar_id \
                                 WHERE k.account_id = ?1 AND k.href = ?2 AND e.href = ?3",
                                params![account.0, calendar_href, href],
                                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                            )
                            .optional()?;
                        match row {
                            Some((id, etag, uid)) => Some(Stored {
                                etag,
                                uid,
                                ics: calendar::event_ics(c, id)?.unwrap_or_default(),
                            }),
                            None => None,
                        }
                    }
                    None => None,
                };
                Ok((
                    has(&calendar_href),
                    dest.as_deref().is_some_and(has),
                    stored,
                ))
            })
            .await
            .map_err(WriteError::internal)?;
        if !known {
            return Err(invalid("calendarHref", "no such writable calendar"));
        }
        if dto.href.is_some() && stored.is_none() {
            return Err(WriteError::NotFound("no such event".to_owned()));
        }
        if let (Some(s), Some(etag)) = (&stored, &dto.etag)
            && s.etag.as_ref() != Some(etag)
        {
            return Err(WriteError::Conflict(
                "the event changed since it was opened; open it again".to_owned(),
            ));
        }
        let before = stored
            .as_ref()
            .and_then(|s| tagmail_calendar::parse(&s.ics).ok());
        let main = before.as_ref().and_then(|c| {
            c.events
                .iter()
                .find(|e| e.recurrence_id.is_none())
                .or_else(|| c.events.first())
                .cloned()
        });
        let organizer = main
            .as_ref()
            .and_then(|e| e.organizer.as_ref())
            .map(|o| o.address.clone());
        let organized = event_fields::organized_by(organizer.as_deref(), &addresses);
        let default = mine.iter().find(|i| i.is_default).or(mine.first());
        let sender = match &organizer {
            _ if !organized => None,
            Some(o) => mine.iter().find(|i| i.address.eq_ignore_ascii_case(o)),
            None => default,
        };
        let no_sender = || {
            invalid(
                "attendees",
                match &organizer {
                    Some(o) => format!("add an identity for {o} to update guests"),
                    None => "add an identity to invite guests".to_owned(),
                },
            )
        };
        let client = config.scheduling() != Some(Scheduling::Server);
        let schedules = client && organized;

        let uid = stored.as_ref().map_or_else(new_uid, |s| s.uid.clone());
        let mut op = CalOp::new(
            dto.op_id.clone(),
            account,
            match dto.kind {
                CalEditKindDto::Put => CalOpKind::Put,
                CalEditKindDto::Delete => CalOpKind::Delete,
                CalEditKindDto::Move => CalOpKind::Move,
            },
            uid.clone(),
        );
        op.calendar_href = Some(dto.calendar_href.clone());
        op.href = dto.href.clone();
        op.if_match = match dto.kind {
            CalEditKindDto::Move => dto.etag.clone(),
            _ => stored.as_ref().and_then(|s| s.etag.clone()),
        };
        let now = OffsetDateTime::now_utc();
        let mut ics = None;
        let mut reply = None;
        let mut generated = Vec::new();
        match dto.kind {
            CalEditKindDto::Put => {
                let wanted = dto
                    .event
                    .as_ref()
                    .ok_or_else(|| invalid("event", "a put needs the event"))?;
                let held_zone = main.as_ref().and_then(|e| e.start.tzid.clone());
                let fields = event_fields::fields_of(wanted, |z| {
                    held_zone.as_deref() == Some(z) || tagmail_calendar::is_known_zone(z)
                })
                .map_err(|(field, why)| invalid(field, why))?;
                let held = before.as_ref().and_then(tagmail_calendar::fields);
                let held_guests: Vec<String> = held
                    .iter()
                    .flat_map(|f| &f.attendees)
                    .map(|a| a.address.to_ascii_lowercase())
                    .collect();
                let guests: Vec<String> = fields
                    .attendees
                    .iter()
                    .map(|a| a.address.to_ascii_lowercase())
                    .collect();
                let same_guests = guests.len() == held_guests.len()
                    && guests.iter().all(|g| held_guests.contains(g));
                if !organized && !same_guests {
                    return Err(invalid(
                        "attendees",
                        "the organizer decides who is invited to this event",
                    ));
                }
                if organized && sender.is_none() && guests.iter().any(|g| !held_guests.contains(g))
                {
                    return Err(no_sender());
                }
                let moved = held
                    .as_ref()
                    .is_some_and(|h| h.start != fields.start || h.end != fields.end);
                let toggled = held
                    .as_ref()
                    .is_some_and(|h| h.start.is_date() != fields.start.is_date());
                if toggled && before.as_ref().is_some_and(tagmail_calendar::has_until) {
                    return Err(invalid(
                        "allDay",
                        "this repeating event has an end date; it cannot switch to or from all day yet",
                    ));
                }
                if moved
                    && before
                        .as_ref()
                        .is_some_and(tagmail_calendar::has_exceptions)
                {
                    return Err(invalid(
                        "start",
                        "this repeating event has exceptions; its time cannot be changed here yet",
                    ));
                }
                let new_organizer = sender.map(|i| EditOrganizer {
                    address: i.address.clone(),
                    name: i.name.clone(),
                });
                let edited = tagmail_calendar::edit(
                    stored.as_ref().map(|s| s.ics.as_slice()),
                    &uid,
                    &fields,
                    new_organizer.as_ref().filter(|_| organizer.is_none()),
                    organized.then_some(client && !fields.attendees.is_empty()),
                    organized,
                    now,
                )
                .map_err(|e| invalid("event", e.to_string()))?;
                let tells = schedules
                    && ((edited.significant && !guests.is_empty()) || !edited.removed.is_empty());
                if tells && sender.is_none() {
                    return Err(no_sender());
                }
                if let Some(identity) = sender.filter(|_| schedules && edited.significant) {
                    let attendees: Vec<String> =
                        fields.attendees.iter().map(|a| a.address.clone()).collect();
                    let to = recipients(&attendees, &identity.address);
                    if !to.is_empty() {
                        let request = tagmail_calendar::request(edited.ics.as_bytes())
                            .map_err(|e| invalid("event", e.to_string()))?;
                        generated.push(
                            itip(
                                &stores,
                                format!("{}{REQUEST_SUFFIX}", op.op_id),
                                identity,
                                to,
                                fields.summary.as_deref(),
                                request,
                                Method::Request,
                            )
                            .await
                            .map_err(compose_error)?,
                        );
                    }
                    let gone = recipients(&edited.removed, &identity.address);
                    if !gone.is_empty() {
                        let cancel = tagmail_calendar::cancel(
                            edited.ics.as_bytes(),
                            Some(&edited.removed),
                            edited.sequence,
                            now,
                        )
                        .map_err(|e| invalid("event", e.to_string()))?;
                        generated.push(
                            itip(
                                &stores,
                                format!("{}{CANCEL_SUFFIX}", op.op_id),
                                identity,
                                gone,
                                fields.summary.as_deref(),
                                cancel,
                                Method::Cancel,
                            )
                            .await
                            .map_err(compose_error)?,
                        );
                    }
                }
                ics = Some(edited.ics);
            }
            CalEditKindDto::Delete if !organized && dto.send_decline == Some(true) => {
                let (Some(calendar), Some(main)) = (&before, &main) else {
                    return Err(invalid("href", "the event cannot be read"));
                };
                let Some(me) = main
                    .attendees
                    .iter()
                    .find(|a| addresses.iter().any(|m| a.is_address(m)))
                else {
                    return Err(invalid("sendDecline", "you are not invited to this event"));
                };
                op.attendee = Some(me.address.clone());
                op.partstat = Some("DECLINED".to_owned());
                op.identity_id = mine
                    .iter()
                    .find(|i| me.is_address(&i.address))
                    .or(default)
                    .map(|i| i.id);
                reply = client.then(|| calendar.clone());
            }
            CalEditKindDto::Delete if !organized => op.reply_cancelled = true,
            CalEditKindDto::Delete => {
                let guests = before
                    .iter()
                    .flat_map(|c| &c.events)
                    .flat_map(|e| &e.attendees)
                    .any(|a| {
                        a.address_is_email && organizer.as_deref().is_none_or(|o| !a.is_address(o))
                    });
                if schedules && guests && sender.is_none() {
                    return Err(no_sender());
                }
                if let (Some(identity), Some(s), Some(main), Some(calendar)) =
                    (sender.filter(|_| schedules), &stored, &main, &before)
                {
                    let mut attendees: Vec<String> = Vec::new();
                    for a in calendar.events.iter().flat_map(|e| &e.attendees) {
                        if a.address_is_email
                            && !attendees.iter().any(|x| x.eq_ignore_ascii_case(&a.address))
                        {
                            attendees.push(a.address.clone());
                        }
                    }
                    let to = recipients(&attendees, &identity.address);
                    if !to.is_empty() {
                        let cancel = tagmail_calendar::cancel(
                            &s.ics,
                            Some(&attendees),
                            main.sequence + 1,
                            now,
                        )
                        .map_err(|e| invalid("event", e.to_string()))?;
                        generated.push(
                            itip(
                                &stores,
                                format!("{}{CANCEL_SUFFIX}", op.op_id),
                                identity,
                                to,
                                main.summary.as_deref(),
                                cancel,
                                Method::Cancel,
                            )
                            .await
                            .map_err(compose_error)?,
                        );
                    }
                }
            }
            CalEditKindDto::Move => {
                if !dest_known {
                    return Err(invalid("destCalendarHref", "no such writable calendar"));
                }
                op.dest_href = dto.dest_calendar_href.clone();
            }
        }
        let op_id = op.op_id.clone();
        let sends = reply.is_some();
        let fresh = record(&stores, op, ics.as_deref(), reply, generated)
            .await
            .map_err(compose_error)?;
        if !fresh {
            return self
                .existing_cal(&op_id)
                .await?
                .ok_or_else(|| WriteError::Conflict("the op id was just used".to_owned()));
        }
        executor::emit(&stores, &op_id).await;
        if sends {
            self.inner.outbox_wake.notify_one();
        }
        self.wake_calendar_ops();
        let dto = self
            .cal_op_dto(op_id)
            .await?
            .ok_or_else(|| WriteError::NotFound("no such op".to_owned()))?;
        Ok(Submitted::Created(dto))
    }
}
