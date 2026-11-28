//! `POST /api/reminders/{alarmKey}/dismiss`: dismissed once for every
//! device, whose notifications close through the `reminder` delete change.
//! `/api/reminders/settings`: the owner's time zone.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::response::Response as HttpResponse;
use tagmail_api_core::Response;
use tagmail_api_core::calendar_dto::{ReminderSettingsDto, ReminderSettingsPatchDto, is_time_zone};
use tagmail_store::alarms;

use crate::auth::{error, internal, no_content, parse};
use crate::http::{AppState, to_http};
use crate::session;

pub async fn dismiss(State(state): State<AppState>, Path(key): Path<String>) -> HttpResponse {
    match state
        .api
        .store()
        .dismiss_reminder(key, session::now())
        .await
    {
        Ok(Some(_)) => no_content(),
        Ok(None) => error(404, "not_found", "no such reminder"),
        Err(e) => internal(e),
    }
}

/// `GET /api/reminders/settings`.
pub async fn settings(State(state): State<AppState>) -> HttpResponse {
    match state.auth.db().read(alarms::owner_zone).await {
        Ok(time_zone) => to_http(Response::json(200, &ReminderSettingsDto { time_zone })),
        Err(e) => internal(e),
    }
}

/// `PATCH /api/reminders/settings`: a new zone recomputes the alarms of
/// floating and all-day events.
pub async fn update_settings(State(state): State<AppState>, body: Bytes) -> HttpResponse {
    let dto: ReminderSettingsPatchDto = match parse(&body) {
        Ok(dto) => dto,
        Err(resp) => return *resp,
    };
    if !is_time_zone(&dto.time_zone) {
        return to_http(Response::json(
            422,
            &tagmail_api_core::dto::ApiErrorDto {
                error: "invalid".to_owned(),
                message: "not an IANA time zone such as Europe/Zurich".to_owned(),
                field: Some("timeZone".to_owned()),
            },
        ));
    }
    let if_unset = dto.if_unset.unwrap_or(false);
    let zone = dto.time_zone;
    let written = state
        .auth
        .db()
        .write(move |tx| {
            let current = alarms::owner_zone(tx)?;
            if if_unset && current.is_some() {
                return Ok((current, false));
            }
            let changed = alarms::set_owner_zone(tx, &zone)?;
            Ok((Some(zone), changed))
        })
        .await;
    match written {
        Ok((time_zone, changed)) => {
            if changed && let Some(calendar) = state.api.calendar() {
                calendar.owner_zone_changed().await;
            }
            to_http(Response::json(200, &ReminderSettingsDto { time_zone }))
        }
        Err(e) => internal(e),
    }
}
