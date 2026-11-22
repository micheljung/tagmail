//! Docker Compose test servers in `test/compose.yml`.
//!
//! Mail is copied into containers with `docker cp` instead of bind mounts so
//! the harness also works against a remote Docker or Podman daemon.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};

use crate::seed;

pub const DOVECOT: &str = "dovecot";
pub const DOVECOT_GMAIL: &str = "dovecot-gmail";
pub const STALWART: &str = "stalwart";
const MAILPIT: &str = "mailpit";
const SERVERS: [&str; 3] = [DOVECOT, DOVECOT_GMAIL, STALWART];

/// uid/gid of `vmail` in the dovecot/dovecot image.
const VMAIL_ID: u64 = 1000;
const MAIL_ROOT: &str = "/srv/vmail";

fn compose_file() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("test")
        .join("compose.yml")
}

fn compose() -> Command {
    let mut cmd = Command::new("docker");
    cmd.arg("compose").arg("-f").arg(compose_file());
    cmd
}

pub(crate) fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("running {cmd:?}"))?;
    if !status.success() {
        bail!("{cmd:?} failed with {status}");
    }
    Ok(())
}

pub(crate) fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running {cmd:?}"))?;
    if !out.status.success() {
        bail!("{cmd:?} failed with {}", out.status);
    }
    Ok(String::from_utf8(out.stdout)?)
}

pub fn up(services: &[&str]) -> Result<()> {
    run(compose()
        .args(["up", "--detach", "--build", "--wait"])
        .args(services)
        .arg(MAILPIT))
}

pub fn down(services: &[&str]) -> Result<()> {
    run(compose()
        .args(["rm", "--stop", "--force", "--volumes"])
        .args(services))?;
    let running = output(compose().args(["ps", "--services"]))?;
    if !running.lines().any(|s| SERVERS.contains(&s)) {
        run(compose().args(["rm", "--stop", "--force", "--volumes", MAILPIT]))?;
    }
    Ok(())
}

/// The id of a running service's container.
pub fn container(service: &str) -> Result<String> {
    let id = output(compose().args(["ps", "--quiet", service]))?;
    let id = id.trim();
    if id.is_empty() {
        bail!("{service} is not running; start it with `cargo xtask dovecot up`");
    }
    Ok(id.to_owned())
}

pub fn load_maildir(service: &str, from: &Path, user: &str) -> Result<()> {
    if !from.join("cur").is_dir() {
        bail!("{} is not a Maildir", from.display());
    }
    let container = container(service)?;

    let mut child = Command::new("docker")
        .args(["cp", "-", &format!("{container}:{MAIL_ROOT}/")])
        .stdin(Stdio::piped())
        .spawn()
        .context("running docker cp")?;
    let stdin = child.stdin.take().expect("piped stdin");
    let mut tar = tar::Builder::new(stdin);
    append_dir(&mut tar, Path::new(user))?;
    append_tree(&mut tar, from, &Path::new(user).join("Maildir"))?;
    tar.into_inner()?.flush()?;
    let status = child.wait()?;
    if !status.success() {
        bail!("docker cp failed with {status}");
    }

    run(compose().args([
        "exec",
        "-T",
        service,
        "doveadm",
        "mailbox",
        "status",
        "-u",
        user,
        "messages unseen",
        "*",
    ]))
}

fn header(kind: tar::EntryType, size: u64, mode: u32, mtime: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(mode);
    header.set_mtime(mtime);
    header.set_uid(VMAIL_ID);
    header.set_gid(VMAIL_ID);
    header
}

fn append_dir<W: std::io::Write>(tar: &mut tar::Builder<W>, path: &Path) -> Result<()> {
    let mut header = header(tar::EntryType::Directory, 0, 0o700, 0);
    tar.append_data(&mut header, path, std::io::empty())?;
    Ok(())
}

fn append_tree<W: std::io::Write>(tar: &mut tar::Builder<W>, src: &Path, dst: &Path) -> Result<()> {
    append_dir(tar, dst)?;
    let mut entries: Vec<_> = fs::read_dir(src)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let target = dst.join(&name);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            append_tree(tar, &entry.path(), &target)?;
        } else if name != seed::MANIFEST {
            let mtime = meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs();
            let mut header = header(tar::EntryType::Regular, meta.len(), 0o600, mtime);
            tar.append_data(&mut header, &target, fs::File::open(entry.path())?)?;
        }
    }
    Ok(())
}
