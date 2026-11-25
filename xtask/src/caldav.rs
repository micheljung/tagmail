//! Calendar test fixtures: per-test users and a seeded Radicale tree.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use time::{Date, Duration, Month};

use crate::seed::Rng;

pub const CAL_USER_COUNT: u32 = 48;
const CAL_USER_PREFIX: &str = "cal";
const CAL_PASSWORD: &str = "engine";
const PROPS_FILE: &str = ".Radicale.props";
const FIRST_DAY: (i32, Month, u8) = (2026, Month::January, 1);
const SPAN_DAYS: u64 = 365;
const RECURRING_RULES: [&str; 4] = [
    "FREQ=DAILY;COUNT=30",
    "FREQ=WEEKLY;COUNT=26",
    "FREQ=MONTHLY;COUNT=12",
    "FREQ=WEEKLY;BYDAY=MO,WE;COUNT=40",
];
const HOURS_PER_DAY: u64 = 10;
const FIRST_HOUR: u64 = 8;
const DURATIONS_MINUTES: [i64; 4] = [30, 60, 90, 120];

pub struct UserFile {
    pub path: PathBuf,
    pub contents: String,
}

fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(relative)
}

fn is_cal_user(line: &str) -> bool {
    line.strip_prefix(CAL_USER_PREFIX)
        .and_then(|rest| rest.split([':', ' ']).next())
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

fn regenerated(path: &Path, cal_line: impl Fn(u32) -> String) -> Result<UserFile> {
    let existing =
        fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut contents: String =
        existing
            .lines()
            .filter(|l| !is_cal_user(l))
            .fold(String::new(), |mut out, l| {
                writeln!(out, "{l}").expect("write to String");
                out
            });
    for n in 1..=CAL_USER_COUNT {
        writeln!(contents, "{}", cal_line(n)).expect("write to String");
    }
    Ok(UserFile {
        path: path.to_owned(),
        contents,
    })
}

pub fn cal_user_files() -> Result<[UserFile; 2]> {
    Ok([
        regenerated(&repo_file("test/dovecot/users"), |n| {
            format!("{CAL_USER_PREFIX}{n}:{{PLAIN}}{CAL_PASSWORD}")
        })?,
        regenerated(&repo_file("test/radicale/users"), |n| {
            format!("{CAL_USER_PREFIX}{n}:{CAL_PASSWORD}")
        })?,
    ])
}

pub fn gen_cal_users(check: bool) -> Result<()> {
    for file in cal_user_files()? {
        if check {
            let current = fs::read_to_string(&file.path)?;
            if current != file.contents {
                bail!(
                    "{} is stale: run `cargo xtask gen-cal-users`",
                    file.path.display()
                );
            }
        } else {
            fs::write(&file.path, &file.contents)?;
            println!("wrote {}", file.path.display());
        }
    }
    Ok(())
}

pub struct SeedCal {
    pub out: PathBuf,
    pub calendars: u32,
    pub events: u32,
    pub recurring_pct: u64,
    pub seed: u64,
}

pub fn seed_cal(opts: &SeedCal) -> Result<()> {
    if opts.calendars == 0 {
        bail!("--calendars must be at least 1");
    }
    if opts.out.exists() && fs::read_dir(&opts.out)?.next().is_some() {
        bail!("{} exists and is not empty", opts.out.display());
    }
    let mut rng = Rng::new(opts.seed);
    let (year, month, day) = FIRST_DAY;
    let first = Date::from_calendar_date(year, month, day)?;
    for c in 0..opts.calendars {
        let name = format!("cal{c}");
        let dir = opts.out.join(&name);
        fs::create_dir_all(&dir)?;
        fs::write(
            dir.join(PROPS_FILE),
            format!(
                r#"{{"tag": "VCALENDAR", "D:displayname": "Calendar {c}", "C:supported-calendar-component-set": "VEVENT"}}"#
            ),
        )?;
        let share = opts.events / opts.calendars + u32::from(c < opts.events % opts.calendars);
        for i in 0..share {
            let uid = format!("seed-{c}-{i}@tagmail.test");
            let ics = event(&mut rng, &uid, i, first, opts.recurring_pct);
            fs::write(dir.join(format!("{uid}.ics")), ics)?;
        }
    }
    println!(
        "seeded {} events in {} calendars into {}",
        opts.events,
        opts.calendars,
        opts.out.display()
    );
    Ok(())
}

fn event(rng: &mut Rng, uid: &str, index: u32, first: Date, recurring_pct: u64) -> String {
    let day = first + Duration::days(rng.below(SPAN_DAYS) as i64);
    let start_hour = FIRST_HOUR + rng.below(HOURS_PER_DAY);
    let minutes = *rng.pick(&DURATIONS_MINUTES);
    let start = day.midnight() + Duration::hours(start_hour as i64);
    let end = start + Duration::minutes(minutes);
    let stamp = |t: time::PrimitiveDateTime| {
        format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            t.year(),
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute(),
            t.second()
        )
    };
    let rrule = if rng.percent(recurring_pct) {
        format!("RRULE:{}\r\n", rng.pick(&RECURRING_RULES))
    } else {
        String::new()
    };
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//tagmail//seed-cal//EN\r\nBEGIN:VEVENT\r\n\
         UID:{uid}\r\nDTSTAMP:{}\r\nDTSTART:{}\r\nDTEND:{}\r\nSUMMARY:Seeded event {index}\r\n{rrule}\
         END:VEVENT\r\nEND:VCALENDAR\r\n",
        stamp(first.midnight()),
        stamp(start),
        stamp(end),
    )
}
