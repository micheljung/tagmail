# Installing Tagmail

Tagmail has two parts:

- **The server** (`tagmail-server`) syncs your IMAP accounts and serves the
  web app. Run one per person, on a machine that is always on.
- **Clients** connect to it: any browser, the web app installed as an app
  (PWA), or the desktop app for Linux, Windows and macOS.

Install the server one of these ways:

| Option | Guide |
|--------|-------|
| Docker / Docker Compose | [deploy/docker/README.md](../deploy/docker/README.md) |
| NixOS module or Nix flake | [deploy/nix/README.md](../deploy/nix/README.md) |
| Release archive (Linux, static binary) | [Release archive](#release-archive) below |

Whichever you choose, put it behind a TLS-terminating
[reverse proxy](#reverse-proxy), then do the [first start](#first-start).

## Release archive

Every [GitHub release](https://github.com/micheljung/tagmail/releases)
has a static server build for Linux:

- `tagmail-server-<version>-x86_64-unknown-linux-musl.tar.gz`
- `tagmail-server-<version>-aarch64-unknown-linux-musl.tar.gz`

Each holds one directory of the same name with `tagmail-server`, the
built web app in `web/`, `LICENSE` and `README.md`. The server serves the
`web` directory next to its binary, so keep the two together (or pass
`--web-dir`). Download the archive and `SHA256SUMS` from the same release,
check and extract it:

```sh
sha256sum -c --ignore-missing SHA256SUMS
sudo tar xzf tagmail-server-<version>-x86_64-unknown-linux-musl.tar.gz -C /opt
```

`SHA256SUMS` covers every asset of the release, including the desktop
installers. Create a system user and the key directory, and start the
server once with `--init-key` to generate the master key:

```sh
sudo useradd --system --home-dir /var/lib/tagmail --create-home tagmail
sudo install -d -m 0700 -o tagmail /etc/tagmail
sudo -u tagmail /opt/tagmail-server-<version>-x86_64-unknown-linux-musl/tagmail-server \
  --data-dir /var/lib/tagmail --init-key \
  --public-origin https://mail.example.com --trusted-proxy 127.0.0.1
```

Later starts need no `--init-key`. Run it under your service manager with
the same options.

### Options

| Option | Default | Meaning |
|--------|---------|---------|
| `--data-dir` | required | Directory for `config.db`, `cache.db` and the message files |
| `--listen` | `127.0.0.1:7878` | Address to listen on. A non-loopback address needs an `https` `--public-origin` |
| `--public-origin` | `http://localhost:<port>` | Origin browsers use, e.g. `https://mail.example.com`. Used for CSRF origin checks, passkeys, `Secure` cookies and pairing links |
| `--trusted-proxy` | none | Proxy address or CIDR whose `X-Forwarded-For` is used for login rate limits. Repeatable or comma-separated; other peers' headers are ignored |
| `--master-key` | see [Master key](#master-key) | Master key file |
| `--init-key` | off | Generate the master key (mode 0600) if it does not exist |
| `--web-dir` | `web` next to the binary, if present | Built web app served at `/` |
| `--poll-interval` | `120` | Seconds between polls of mailboxes other than INBOX (INBOX uses IDLE) |
| `--max-sse-clients` | `8` | Concurrent event streams (open tabs and apps) |
| `--worker-threads` | `2` | Async worker threads |
| `--max-blocking-threads` | `8` | Upper bound of the blocking pool (SQLite reads, password hashing) |
| `--log-format` | `text` | `text` or `json`. The level comes from `RUST_LOG` (default `info`) |
| `--insecure-http` | off | Development only: allow a non-loopback `--listen` with a plain `http` origin |
| `--insecure-plain-imap` | off | Development only: connect to IMAP and SMTP without TLS for every account, sending passwords in cleartext |

`tagmail-server --help` lists the same.

### Master key

IMAP and SMTP passwords, and the Web Push key, are encrypted in
`config.db` with a master key. The server looks for it in this order:

1. `--master-key`
2. `$CREDENTIALS_DIRECTORY/master.key` (systemd `LoadCredential=`)
3. `$TAGMAIL_MASTER_KEY_FILE` (e.g. a Docker secret)
4. `/etc/tagmail/master.key`

Without a key file the server refuses to start; `--init-key` generates
one. You can also create it yourself: the file holds 32 random bytes, raw,
as 64 hex digits or in base64, e.g. `openssl rand -hex 32`.

Keep the key outside the data directory. The server logs a warning at
every start if it is inside, and one if other users can read it. Back the
key up separately from the data, see [backup.md](backup.md).

## Reverse proxy

The server speaks plain HTTP and does not terminate TLS. Put a reverse
proxy with a certificate in front, pass its address with
`--public-origin https://<host>` and list the proxy with
`--trusted-proxy`. Requirements:

- Forward `X-Forwarded-For`, so login rate limits apply per client.
- Do not buffer `/api/events`: it is a server-sent event stream with a
  heartbeat every 25 seconds.
- Allow request bodies of at least 26 MB: one attachment upload may be up
  to 25 MiB. A message may carry 35 MiB of attachments in total, uploaded
  one by one.
- If another login (an OAuth proxy, basic auth) sits in front of Tagmail,
  let these through without it. The desktop app pairs and syncs with its
  own device token and cannot follow a redirect to a login page:
  - `POST /api/pairing/exchange` and `GET /api/version`
  - every request under `/api/` that carries an `Authorization: Bearer`
    header. Tagmail checks the token itself and rejects bearer tokens sent
    by browsers.

  Keep the web app, `/api/auth/*` (login, passkeys, setup) and requests
  without a bearer token behind the proxy. Tagmail answers 401 to any
  `/api` request it cannot authenticate, so nothing else becomes reachable.
- Serve Tagmail at the root of its own host name, not under a path.

### Caddy

Caddy gets a certificate on its own, forwards `X-Forwarded-For` and does
not buffer event streams or limit request bodies by default:

```
mail.example.com {
	reverse_proxy 127.0.0.1:7878
}
```

### nginx

```nginx
server {
    listen 443 ssl;
    http2 on;
    server_name mail.example.com;

    ssl_certificate     /etc/letsencrypt/live/mail.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/mail.example.com/privkey.pem;

    client_max_body_size 30m;

    location / {
        proxy_pass http://127.0.0.1:7878;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_buffering off;
    }
}
```

## First start

On its first start, and on every start until setup is done, the server
logs a one-time setup token:

```
no owner yet: open https://mail.example.com and finish setup with the one-time setup token <token>
```

Open the address, paste the token and choose the owner password (at least
8 characters). Then:

1. **Settings → Accounts → Add an account:** IMAP and SMTP host, port,
   security and password. Use an app password where the provider offers
   one; see [servers.md](servers.md) for provider notes.
2. **Settings → Devices & sessions → Passkeys** (optional): add a passkey.
   Passkeys need a host name origin; `localhost` works, IP addresses do
   not.

The first sync shows the newest INBOX mail first and fetches older mail
and message bodies in the background.

## Desktop app

The desktop app keeps a local copy of your mail for offline reading,
search and compose, shows native notifications, a tray icon with the
unread count, and handles `mailto:` links. It needs a running server.

Download the installer for your system from the
[releases page](https://github.com/micheljung/tagmail/releases) and check
it against `SHA256SUMS` as [above](#release-archive):

| System | Files |
|--------|-------|
| Linux | `.AppImage`, `.deb`, `.rpm` |
| Windows | `.msi`, `-setup.exe` |
| macOS (Apple silicon and Intel) | `.dmg` |

The installers are **not signed**, so the system warns before the first
start.

**Linux** (glibc 2.35 or newer, e.g. Ubuntu 22.04, Debian 12, Fedora 36)

- AppImage: `chmod +x Tagmail_*.AppImage`, then run it.
- Debian, Ubuntu: `sudo apt install ./Tagmail_*.deb`
- Fedora, openSUSE: `sudo dnf install ./Tagmail-*.rpm` (or `zypper`)

**Windows:** run the installer. When SmartScreen shows "Windows protected
your PC", choose **More info → Run anyway**.

**macOS:** open the `.dmg` and drag Tagmail to Applications. The first
start is blocked as from an unidentified developer. Either open
**System Settings → Privacy & Security** and choose **Open Anyway**
(on older macOS versions, right-click the app and choose **Open**), or
remove the quarantine attribute:

```sh
xattr -dr com.apple.quarantine /Applications/Tagmail.app
```

### Pairing

The desktop app signs in with a device token instead of the password:

1. In the web app open **Settings → Devices & sessions → Pair a device**.
   It shows an 8-character code and a QR code, valid once for 5 minutes.
2. In the desktop app enter the server address and the code under
   **Connect to a server**, or open the QR code's `tagmail://pair` link on
   the same computer.

The token is stored in the system keychain (Secret Service, macOS
Keychain, Windows Credential Manager). Without one it is kept in a file
only your user can read, and the app shows a warning. Revoke a desktop
under **Paired devices** on the same page.

In the desktop app, **This device** in the menu sets how many days of
mail are kept for offline reading and search (90 by default; starred
mail always), the disk space for full messages, notifications, autostart
and keeping the app in the tray when the window closes. Older mail stays
listed and opens or searches while the server is reachable.

## Installing the web app (PWA)

The web app can be installed on phones, tablets and desktops and can show
notifications for new mail. See [pwa.md](pwa.md).

## Updating

Replace the binary, image or package and restart. Databases are migrated
on start. The desktop app refuses to sync with a server that is too old
or too new for it and asks for an update.
