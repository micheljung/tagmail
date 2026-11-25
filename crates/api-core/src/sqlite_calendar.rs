//! The calendar half of [`crate::sqlite_store::SqliteStore`]: CalDAV
//! settings and calendar preferences in `config.db`, the calendar cache in
//! `cache.db`.

use std::collections::HashMap;

use tagmail_core::AccountId;
use tagmail_store::calendar::{
    self, CaldavConfig, CaldavStatus, CalendarPref, CalendarRow, Scheduling,
};
use tagmail_store::rusqlite::{Connection, OptionalExtension};

use crate::calendar_dto::{
    CaldavSettingsDto, CaldavStatusDto, CalendarDto, CalendarTargetDto, EventStatusDto,
    PartstatDto, SchedulingDto, SchedulingOverrideDto, StoredEventDto,
};
use crate::dto::AccountStateDto;
use crate::store::{CaldavSettingsUpdate, CalendarPatch, InvitationContext, StoredCopy};

pub(crate) fn scheduling_dto(s: Scheduling) -> SchedulingDto {
    match s {
        Scheduling::Server => SchedulingDto::Server,
        Scheduling::Client => SchedulingDto::Client,
    }
}

fn scheduling(s: SchedulingDto) -> Scheduling {
    match s {
        SchedulingDto::Server => Scheduling::Server,
        SchedulingDto::Client => Scheduling::Client,
    }
}

pub(crate) fn status_dto(s: CaldavStatus) -> CaldavStatusDto {
    match s {
        CaldavStatus::Unconfigured => CaldavStatusDto::Unconfigured,
        CaldavStatus::NeedsConfirmation => CaldavStatusDto::NeedsConfirmation,
        CaldavStatus::Ok => CaldavStatusDto::Ok,
        CaldavStatus::Error => CaldavStatusDto::Error,
    }
}

pub(crate) fn settings_dto(c: &CaldavConfig) -> CaldavSettingsDto {
    CaldavSettingsDto {
        url: c.url.clone(),
        username: c.username.clone(),
        has_password: c.secret.is_some(),
        allow_insecure: c.allow_insecure,
        scheduling: match c.scheduling {
            None => SchedulingOverrideDto::Auto,
            Some(Scheduling::Server) => SchedulingOverrideDto::Server,
            Some(Scheduling::Client) => SchedulingOverrideDto::Client,
        },
        scheduling_detected: c.scheduling_detected.map(scheduling_dto),
        fallback_account_id: c.fallback_account.map(|a| a.0),
        default_calendar_href: c.default_calendar_href.clone(),
        authserv_id: c.authserv_id.clone(),
        authserv_id_suggestion: None,
        confirmed_host: c.confirmed_host.clone(),
        status: status_dto(c.status),
        last_error: c.last_error.clone(),
    }
}

fn account_status(conn: &Connection, id: AccountId) -> tagmail_store::Result<Option<String>> {
    Ok(conn
        .query_row("SELECT status FROM account WHERE id = ?1", [id.0], |r| {
            r.get(0)
        })
        .optional()?)
}

/// The account's settings, defaults when it has none; `None` without the
/// account.
pub(crate) fn settings(
    conn: &Connection,
    id: AccountId,
) -> tagmail_store::Result<Option<CaldavConfig>> {
    if account_status(conn, id)?.is_none() {
        return Ok(None);
    }
    Ok(Some(calendar::load_caldav(conn, id)?.unwrap_or_default()))
}

/// Applies `update` (password already sealed into `secret`) and saves.
pub(crate) fn save_settings(
    conn: &Connection,
    id: AccountId,
    update: CaldavSettingsUpdate,
    secret: Option<tagmail_store::Sealed>,
) -> tagmail_store::Result<Option<CaldavConfig>> {
    let status = account_status(conn, id)?;
    if status.is_none_or(|s| s == AccountStateDto::Deleting.as_str()) {
        return Ok(None);
    }
    let mut config = calendar::load_caldav(conn, id)?.unwrap_or_default();
    if config.url != update.url {
        config = CaldavConfig {
            authserv_id: config.authserv_id,
            ..CaldavConfig::default()
        };
    }
    config.secret = match (&update.username, secret) {
        (None, _) => None,
        (Some(_), Some(sealed)) => Some(sealed),
        (Some(_), None) => config.secret,
    };
    config.url = update.url;
    config.username = update.username;
    config.allow_insecure = update.allow_insecure;
    config.scheduling = update.scheduling.map(scheduling);
    config.fallback_account = update.fallback_account;
    config.default_calendar_href = update.default_calendar_href;
    if let Some(authserv_id) = update.authserv_id {
        config.authserv_id = authserv_id;
    }
    calendar::save_caldav(conn, id, &config)?;
    Ok(Some(config))
}

/// Default calendar href of every account that set one.
pub(crate) fn default_hrefs(
    conn: &Connection,
) -> tagmail_store::Result<HashMap<AccountId, String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT account_id, default_calendar_href FROM caldav \
         WHERE default_calendar_href IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| Ok((AccountId(r.get(0)?), r.get(1)?)))?;
    Ok(rows.collect::<tagmail_store::rusqlite::Result<_>>()?)
}

pub(crate) fn calendar_dto(
    row: &CalendarRow,
    pref: Option<&CalendarPref>,
    default: Option<&String>,
) -> CalendarDto {
    CalendarDto {
        account_id: row.account.0,
        href: row.href.clone(),
        display_name: row.display_name.clone(),
        color: pref
            .and_then(|p| p.color.clone())
            .or_else(|| row.color.clone()),
        visible: pref.is_none_or(|p| p.visible),
        sort: pref.and_then(|p| p.sort),
        writable: row.writable,
        is_default: default == Some(&row.href),
    }
}

/// Applies the given fields of `patch` to a calendar known to exist;
/// returns its preferences and the account's default calendar.
pub(crate) fn patch_pref(
    conn: &Connection,
    patch: &CalendarPatch,
) -> tagmail_store::Result<(CalendarPref, Option<String>)> {
    calendar::update_calendar_pref(
        conn,
        patch.account,
        &patch.href,
        patch.color.as_ref().map(Option::as_deref),
        patch.visible,
        patch.sort,
    )?;
    let default = calendar::load_caldav(conn, patch.account)?.and_then(|c| c.default_calendar_href);
    let is_default = default.as_deref() == Some(patch.href.as_str());
    match patch.default {
        Some(true) if !is_default => {
            calendar::set_default_calendar(conn, patch.account, Some(&patch.href))?;
        }
        Some(false) if is_default => calendar::set_default_calendar(conn, patch.account, None)?,
        _ => {}
    }
    let pref = calendar::calendar_prefs(conn)?
        .remove(&(patch.account, patch.href.clone()))
        .unwrap_or_else(|| CalendarPref::new(patch.href.clone()));
    let default = calendar::load_caldav(conn, patch.account)?.and_then(|c| c.default_calendar_href);
    Ok((pref, default))
}

fn partstat_dto(value: &str) -> PartstatDto {
    match value.to_ascii_uppercase().as_str() {
        "NEEDS-ACTION" => PartstatDto::NeedsAction,
        "ACCEPTED" => PartstatDto::Accepted,
        "DECLINED" => PartstatDto::Declined,
        "TENTATIVE" => PartstatDto::Tentative,
        "DELEGATED" => PartstatDto::Delegated,
        _ => PartstatDto::Other,
    }
}

fn event_status_dto(value: &str) -> Option<EventStatusDto> {
    match value.to_ascii_uppercase().as_str() {
        "TENTATIVE" => Some(EventStatusDto::Tentative),
        "CONFIRMED" => Some(EventStatusDto::Confirmed),
        "CANCELLED" => Some(EventStatusDto::Cancelled),
        _ => None,
    }
}

/// The accounts an invitation to `account` maps to, and the config of
/// each, read from `config.db`.
pub(crate) fn mapped_accounts(
    conn: &Connection,
    account: AccountId,
) -> tagmail_store::Result<Vec<(AccountId, CaldavConfig)>> {
    let own = calendar::load_caldav(conn, account)?.unwrap_or_default();
    let fallback = own.fallback_account;
    let mut mapped = vec![(account, own)];
    if let Some(fallback) = fallback {
        mapped.push((
            fallback,
            calendar::load_caldav(conn, fallback)?.unwrap_or_default(),
        ));
    }
    Ok(mapped)
}

/// Stored copies of `uids` and the target, from `cache.db`.
pub(crate) fn invitation_context(
    conn: &Connection,
    mapped: &[(AccountId, CaldavConfig)],
    uids: &[String],
) -> tagmail_store::Result<InvitationContext> {
    let accounts: Vec<AccountId> = mapped.iter().map(|(a, _)| *a).collect();
    let (target, scheduling) = mapped
        .iter()
        .find_map(|(account, config)| {
            let href = config.default_calendar_href.clone()?;
            let target = CalendarTargetDto {
                account_id: account.0,
                calendar_href: href,
            };
            Some((Some(target), config.scheduling().map(scheduling_dto)))
        })
        .unwrap_or((None, None));
    let mut stored = Vec::new();
    for uid in uids {
        for copy in calendar::events_by_uid(conn, &accounts, uid)? {
            stored.push(StoredCopy {
                uid: uid.clone(),
                dtstamp: copy.event.dtstamp,
                stored: StoredEventDto {
                    account_id: copy.account.0,
                    calendar_href: copy.calendar_href,
                    partstat: copy.event.my_partstat.as_deref().map(partstat_dto),
                    sequence: copy.event.sequence,
                    status: copy.event.status.as_deref().and_then(event_status_dto),
                },
            });
        }
    }
    Ok(InvitationContext {
        stored,
        target,
        scheduling,
    })
}

#[cfg(all(test, feature = "calendar"))]
mod tests {
    use tagmail_store::calendar::{EventRow, InstanceRow};
    use tagmail_store::rusqlite::params;
    use tagmail_store::{BlobStore, CacheDb, ConfigDb, MasterKey};
    use tempfile::TempDir;

    use super::*;
    use crate::Secret;
    use crate::calendar_dto::PartstatDto;
    use crate::invitation::card;
    use crate::mime::reduce::calendar_parts;
    use crate::sqlite_store::SqliteStore;
    use crate::store::Store;

    const INVITED: AccountId = AccountId(1);
    const CALENDARS: AccountId = AccountId(2);
    const WORK: &str = "/cal/work/";
    const STORED_SEQUENCE: u32 = 3;

    async fn store(dir: &TempDir) -> SqliteStore {
        let config = ConfigDb::open(&dir.path().join("config.db")).unwrap();
        let cache = CacheDb::open(&dir.path().join("cache.db")).unwrap();
        let blobs = BlobStore::open(&dir.path().join("blobs")).unwrap();
        let store = SqliteStore::open(config, cache, blobs, &MasterKey::generate())
            .await
            .unwrap();
        store
            .config()
            .write(|tx| {
                for id in [INVITED.0, CALENDARS.0] {
                    tx.execute(
                        "INSERT INTO account (id, name, imap_host, imap_port, imap_tls, \
                         smtp_host, smtp_port, smtp_tls, username, secret_ct, secret_nonce) \
                         VALUES (?1, 'a', 'h', 993, 'tls', 'h', 465, 'tls', 'u', x'00', x'00')",
                        params![id],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        store
    }

    fn invitation(sequence: u32) -> Vec<u8> {
        format!(
            "From: <boss@example.com>\r\nSubject: Invitation\r\n\
             Content-Type: text/calendar; method=REQUEST\r\n\r\n\
             BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\n\
             DTSTAMP:20261101T120000Z\r\nSEQUENCE:{sequence}\r\nDTSTART:20261201T090000Z\r\n\
             DTEND:20261201T100000Z\r\nSUMMARY:Review\r\nORGANIZER:mailto:boss@example.com\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:me@example.com\r\nEND:VEVENT\r\n\
             END:VCALENDAR\r\n"
        )
        .into_bytes()
    }

    async fn card(store: &SqliteStore, sequence: u32) -> crate::calendar_dto::InvitationDto {
        let raw = invitation(sequence);
        let (mut dto, versions) = card::invitation(&calendar_parts(raw.as_slice()).unwrap())
            .unwrap_or_else(|r| panic!("{}", String::from_utf8_lossy(&r.body)));
        let uids = versions.iter().map(|v| v.uid.clone()).collect();
        let context = store.invitation_context(INVITED, uids).await.unwrap();
        card::apply_context(&mut dto, &versions, context);
        dto
    }

    #[tokio::test]
    async fn the_card_reports_the_stored_copy_in_the_fallback_calendar() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir).await;
        let before = card(&store, 2).await;
        assert_eq!(
            (before.stored, before.target, before.outdated),
            (None, None, false)
        );

        store
            .save_caldav_settings(
                INVITED,
                CaldavSettingsUpdate {
                    fallback_account: Some(CALENDARS),
                    ..CaldavSettingsUpdate::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        store
            .config()
            .write(|tx| {
                calendar::save_caldav(
                    tx,
                    CALENDARS,
                    &CaldavConfig {
                        scheduling_detected: Some(Scheduling::Client),
                        ..CaldavConfig::default()
                    },
                )
            })
            .await
            .unwrap();
        store
            .cache()
            .write(|tx| {
                let id = calendar::upsert_calendar(tx, CALENDARS, WORK, Some("Work"), None, true)?;
                calendar::upsert_calendar(tx, CALENDARS, "/cal/home/", None, None, true)?;
                let event = EventRow {
                    href: format!("{WORK}u1.ics"),
                    uid: "u1".into(),
                    sequence: STORED_SEQUENCE,
                    status: Some("CONFIRMED".into()),
                    my_partstat: Some("ACCEPTED".into()),
                    ..EventRow::default()
                };
                let instance = InstanceRow {
                    start: 0,
                    end: 1,
                    recurrence_id: None,
                    all_day: false,
                };
                calendar::put_event(tx, id, &event, b"ICS", &[instance])?;
                Ok(())
            })
            .await
            .unwrap();
        let patched = store
            .update_calendar(CalendarPatch {
                account: CALENDARS,
                href: WORK.into(),
                color: Some(Some("#00ff00".into())),
                visible: None,
                default: Some(true),
                sort: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert!(patched.is_default);
        assert_eq!(patched.color.as_deref(), Some("#00ff00"));

        let older = card(&store, 2).await;
        assert_eq!(
            older.stored,
            Some(StoredEventDto {
                account_id: CALENDARS.0,
                calendar_href: WORK.into(),
                partstat: Some(PartstatDto::Accepted),
                sequence: STORED_SEQUENCE,
                status: Some(EventStatusDto::Confirmed),
            })
        );
        assert!(older.outdated);
        assert_eq!(
            older.target,
            Some(CalendarTargetDto {
                account_id: CALENDARS.0,
                calendar_href: WORK.into(),
            })
        );
        assert_eq!(older.scheduling, Some(SchedulingDto::Client));
        assert!(!card(&store, STORED_SEQUENCE).await.outdated);
    }

    #[tokio::test]
    async fn settings_seal_the_password_and_a_new_url_drops_discovery() {
        let dir = TempDir::new().unwrap();
        let store = store(&dir).await;
        let defaults = store.caldav_settings(INVITED).await.unwrap().unwrap();
        assert_eq!(defaults.status, CaldavStatusDto::Unconfigured);
        assert_eq!(defaults.authserv_id, None);
        assert_eq!(store.caldav_settings(AccountId(9)).await.unwrap(), None);

        let saved = store
            .save_caldav_settings(
                INVITED,
                CaldavSettingsUpdate {
                    url: Some("https://dav.example.com/".into()),
                    username: Some("dav".into()),
                    password: Some(Secret::new("pw".into())),
                    scheduling: Some(SchedulingDto::Server),
                    ..CaldavSettingsUpdate::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert!(saved.has_password);
        assert_eq!(saved.scheduling, SchedulingOverrideDto::Server);
        let sealed = store
            .config()
            .read(|c| calendar::load_caldav(c, INVITED))
            .await
            .unwrap()
            .unwrap()
            .secret
            .unwrap();
        assert_eq!(*store.vault().open_caldav(INVITED, &sealed).unwrap(), "pw");

        store
            .config()
            .write(|tx| {
                let mut c = calendar::load_caldav(tx, INVITED)?.unwrap();
                c.home_url = Some("https://dav.example.com/home/".into());
                c.status = CaldavStatus::Ok;
                calendar::save_caldav(tx, INVITED, &c)
            })
            .await
            .unwrap();
        let moved = store
            .save_caldav_settings(
                INVITED,
                CaldavSettingsUpdate {
                    url: Some("https://other.example.com/".into()),
                    ..CaldavSettingsUpdate::default()
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (moved.status, moved.has_password),
            (CaldavStatusDto::Unconfigured, false)
        );
        let home = store
            .config()
            .read(|c| calendar::load_caldav(c, INVITED))
            .await
            .unwrap()
            .unwrap()
            .home_url;
        assert_eq!(home, None);
    }
}
