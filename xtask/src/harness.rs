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
pub const RADICALE: &str = "radicale";
const MAILPIT: &str = "mailpit";
const SERVERS: [&str; 4] = [DOVECOT, DOVECOT_GMAIL, STALWART, RADICALE];

/// uid/gid of `vmail` in the dovecot/dovecot image.
const VMAIL_ID: u64 = 1000;
const MAIL_ROOT: &str = "/srv/vmail";
/// uid/gid of `radicale` in test/radicale/Dockerfile.
const RADICALE_ID: u64 = 1000;
const RADICALE_STORAGE: &str = "/data/collections";
const RADICALE_ROOT: &str = "collection-root";

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
    copy_tree(
        &container,
        MAIL_ROOT,
        VMAIL_ID,
        &[Path::new(user)],
        from,
        &Path::new(user).join("Maildir"),
    )?;

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

/// Copies the seeded calendars in `from` (one directory per calendar) to
/// `user`'s collections in Radicale.
pub fn load_calendars(from: &Path, user: &str) -> Result<()> {
    if !from.is_dir() {
        bail!("{} is not a directory", from.display());
    }
    let container = container(RADICALE)?;
    let user_dir = Path::new(RADICALE_ROOT).join(user);
    copy_tree(
        &container,
        RADICALE_STORAGE,
        RADICALE_ID,
        &[Path::new(RADICALE_ROOT), &user_dir],
        from,
        &user_dir,
    )?;
    println!("loaded {} into {user}", from.display());
    Ok(())
}

/// `docker cp` of `from` to `target` below `dest`, owned by `owner`, after
/// creating the `parents` directories.
fn copy_tree(
    container: &str,
    dest: &str,
    owner: u64,
    parents: &[&Path],
    from: &Path,
    target: &Path,
) -> Result<()> {
    let mut child = Command::new("docker")
        .args(["cp", "-", &format!("{container}:{dest}/")])
        .stdin(Stdio::piped())
        .spawn()
        .context("running docker cp")?;
    let stdin = child.stdin.take().expect("piped stdin");
    let mut tar = tar::Builder::new(stdin);
    for parent in parents {
        append_dir(&mut tar, parent, owner)?;
    }
    append_tree(&mut tar, from, target, owner)?;
    tar.into_inner()?.flush()?;
    let status = child.wait()?;
    if !status.success() {
        bail!("docker cp failed with {status}");
    }
    Ok(())
}

fn header(kind: tar::EntryType, size: u64, mode: u32, mtime: u64, owner: u64) -> tar::Header {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(kind);
    header.set_size(size);
    header.set_mode(mode);
    header.set_mtime(mtime);
    header.set_uid(owner);
    header.set_gid(owner);
    header
}

fn append_dir<W: std::io::Write>(tar: &mut tar::Builder<W>, path: &Path, owner: u64) -> Result<()> {
    let mut header = header(tar::EntryType::Directory, 0, 0o700, 0, owner);
    tar.append_data(&mut header, path, std::io::empty())?;
    Ok(())
}

fn append_tree<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    src: &Path,
    dst: &Path,
    owner: u64,
) -> Result<()> {
    append_dir(tar, dst, owner)?;
    let mut entries: Vec<_> = fs::read_dir(src)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let target = dst.join(&name);
        let meta = entry.metadata()?;
        if meta.is_dir() {
            append_tree(tar, &entry.path(), &target, owner)?;
        } else if name != seed::MANIFEST {
            let mtime = meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs();
            let mut header = header(tar::EntryType::Regular, meta.len(), 0o600, mtime, owner);
            tar.append_data(&mut header, &target, fs::File::open(entry.path())?)?;
        }
    }
    Ok(())
}
