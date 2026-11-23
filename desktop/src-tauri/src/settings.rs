//! Desktop settings: defaults, validation and their `replica-durable.db`
//! meta keys.

use tagmail_api_core::api;
use tagmail_api_core::desktop_dto::{DesktopSettingsDto, DesktopSettingsPatchDto};
use tagmail_api_core::dto::ApiErrorDto;
use tagmail_api_core::replica_dto::DEFAULT_WINDOW_DAYS;
use tagmail_replica::window::DEFAULT_RAW_CAP_BYTES;
use tagmail_replica::{Durable, Settings};

const MIB: u64 = 1024 * 1024;
const WINDOW_DAYS: std::ops::RangeInclusive<u32> = 7..=3650;
const MAX_RAW_CAP_MB: u64 = 1024 * 1024;

const WINDOW_KEY: &str = "windowDays";
const RAW_CAP_KEY: &str = "rawCapBytes";
const NOTIFY_KEY: &str = "notify";
const NOTIFY_CONTENT_KEY: &str = "notifyContent";
const AUTOSTART_KEY: &str = "autostart";
const CLOSE_TO_TRAY_KEY: &str = "closeToTray";

pub fn defaults() -> DesktopSettingsDto {
    DesktopSettingsDto {
        window_days: DEFAULT_WINDOW_DAYS,
        raw_cap_mb: DEFAULT_RAW_CAP_BYTES / MIB,
        notify: true,
        notify_content: true,
        autostart: false,
        close_to_tray: true,
    }
}

fn invalid(field: &str, message: &str) -> api::Response {
    api::Response::json(
        422,
        &ApiErrorDto {
            error: "invalid".to_owned(),
            message: message.to_owned(),
            field: Some(field.to_owned()),
        },
    )
}

pub fn apply(
    current: &DesktopSettingsDto,
    patch: DesktopSettingsPatchDto,
) -> Result<DesktopSettingsDto, api::Response> {
    let mut next = current.clone();
    if let Some(days) = patch.window_days {
        if !WINDOW_DAYS.contains(&days) {
            return Err(invalid("windowDays", "7 to 3650 days"));
        }
        next.window_days = days;
    }
    if let Some(mb) = patch.raw_cap_mb {
        if mb > MAX_RAW_CAP_MB {
            return Err(invalid("rawCapMb", "at most 1 TiB"));
        }
        next.raw_cap_mb = mb;
    }
    next.notify = patch.notify.unwrap_or(next.notify);
    next.notify_content = patch.notify_content.unwrap_or(next.notify_content);
    next.autostart = patch.autostart.unwrap_or(next.autostart);
    next.close_to_tray = patch.close_to_tray.unwrap_or(next.close_to_tray);
    Ok(next)
}

pub fn replica_settings(settings: &DesktopSettingsDto) -> Settings {
    Settings {
        window_days: settings.window_days,
        raw_cap_bytes: settings.raw_cap_mb.saturating_mul(MIB),
    }
}

fn read<T: std::str::FromStr>(durable: &Durable, key: &str) -> tagmail_replica::Result<Option<T>> {
    Ok(durable.meta(key)?.and_then(|v| v.parse().ok()))
}

pub fn load(durable: &Durable) -> tagmail_replica::Result<DesktopSettingsDto> {
    let d = defaults();
    Ok(DesktopSettingsDto {
        window_days: read(durable, WINDOW_KEY)?.unwrap_or(d.window_days),
        raw_cap_mb: read::<u64>(durable, RAW_CAP_KEY)?.map_or(d.raw_cap_mb, |b| b / MIB),
        notify: read(durable, NOTIFY_KEY)?.unwrap_or(d.notify),
        notify_content: read(durable, NOTIFY_CONTENT_KEY)?.unwrap_or(d.notify_content),
        autostart: read(durable, AUTOSTART_KEY)?.unwrap_or(d.autostart),
        close_to_tray: read(durable, CLOSE_TO_TRAY_KEY)?.unwrap_or(d.close_to_tray),
    })
}

pub fn save(durable: &Durable, settings: &DesktopSettingsDto) -> tagmail_replica::Result<()> {
    durable.set_meta(WINDOW_KEY, &settings.window_days.to_string())?;
    durable.set_meta(RAW_CAP_KEY, &(settings.raw_cap_mb * MIB).to_string())?;
    durable.set_meta(NOTIFY_KEY, &settings.notify.to_string())?;
    durable.set_meta(NOTIFY_CONTENT_KEY, &settings.notify_content.to_string())?;
    durable.set_meta(AUTOSTART_KEY, &settings.autostart.to_string())?;
    durable.set_meta(CLOSE_TO_TRAY_KEY, &settings.close_to_tray.to_string())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn validates_and_round_trips_through_the_durable_store() {
        let dir = TempDir::new().unwrap();
        let durable = Durable::open(&dir.path().join("d.db")).unwrap();
        assert_eq!(load(&durable).unwrap(), defaults());
        let patched = apply(
            &defaults(),
            DesktopSettingsPatchDto {
                window_days: Some(30),
                raw_cap_mb: Some(512),
                notify_content: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
        save(&durable, &patched).unwrap();
        assert_eq!(load(&durable).unwrap(), patched);
        assert_eq!(replica_settings(&patched).raw_cap_bytes, 512 * MIB);
        for (patch, field) in [
            (
                DesktopSettingsPatchDto {
                    window_days: Some(1),
                    ..Default::default()
                },
                "windowDays",
            ),
            (
                DesktopSettingsPatchDto {
                    raw_cap_mb: Some(u64::MAX),
                    ..Default::default()
                },
                "rawCapMb",
            ),
        ] {
            let err = apply(&defaults(), patch).unwrap_err();
            assert_eq!(err.status, 422);
            let body: serde_json::Value = serde_json::from_slice(&err.body).unwrap();
            assert_eq!(body["field"], field);
        }
    }
}
