# Security model

To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Threat model

Tagmail is a single-owner server. It protects:

- your mail and account passwords against anyone without your owner
  password, passkey, browser session or paired device;
- your account passwords in `config.db` and its backups, as long as the
  master key is not stored with them;
- the web app against hostile email content (scripts, tracking images,
  requests to your internal network).

It does not protect against an attacker who controls the host the server
runs on. The server decrypts account passwords unattended at start, so
whoever can read both the data directory and the master key, or the
server's memory, can read them. A mode that asks for a password at every
start is not part of v1.

Mail itself is stored as the IMAP server delivers it: `cache.db`,
`blobs/` and the desktop app's offline copy are not encrypted. Use disk
encryption where that matters.

## Encryption at rest

IMAP and SMTP passwords are sealed with XChaCha20-Poly1305 under a random
data key, with the account id as associated data. The data key is stored
in `config.db`, wrapped by the master key, which lives outside the data
directory ([install.md](install.md#master-key)). The Web Push signing key
is sealed the same way. The server logs a warning at every start if the
key file is inside the data directory or readable by other users.
Secrets are zeroed after use and never logged.

The owner password is stored as an argon2id hash (19 MiB, t=2, p=1).
Session tokens, device tokens and pairing codes are stored as SHA-256
hashes.

## Network

The server serves plain HTTP on `127.0.0.1:7878` by default and refuses a
non-loopback `--listen` address unless `--public-origin` is `https` (TLS
terminated by a reverse proxy). `--insecure-http` overrides that for
development only.

IMAP and SMTP connections use TLS or STARTTLS (rustls), verified against
the Mozilla root certificates built into the binary (`webpki-roots`), not
the system's store. `--insecure-plain-imap` turns TLS off for every account of
the server and is meant for local test servers only.

## Login and sessions

On first start the server logs a one-time setup token; setup needs it
together with the new owner password. Logins hash one password at a time
and are rate limited per client IP (10 attempts, then one per 10 s). A
global limit (600, then ten per second) is only a backstop: an attacker
with hundreds of addresses can still delay password logins until it
refills. The client IP is taken from `X-Forwarded-For` only for peers
listed with `--trusted-proxy`.

Passkeys can be added in Settings → Devices & sessions. They are bound to
the host name of `--public-origin` (`localhost` works, IP addresses do
not).

Browser sessions are random 256-bit tokens in an
`HttpOnly; SameSite=Strict` cookie (`Secure` with an `https` origin).
They expire after 30 days without use or 90 days after login and can be
listed and revoked in Settings. Open event streams end when their session
or device does.

Every `/api` route except `/api/version`, setup, login and the pairing
exchange requires a session or a device token.

## CSRF

Besides `SameSite=Strict`, every state-changing request must send
`X-Requested-With: tagmail`. Browsers cannot add that header cross-origin
without a CORS preflight, and the server allows no CORS. A present
`Origin` must match the public origin, and `Sec-Fetch-Site` must be
`same-origin` or `none`. Behind a web dev server that proxies `/api`,
pass the dev server's origin as `--public-origin`.

## Desktop pairing and device tokens

The web app shows an 8-character pairing code (and QR code), valid once
for 5 minutes. The desktop app exchanges it for a device token, which
only its Rust side sends, as `Authorization: Bearer`; the web view never
sees it. Requests from browsers cannot use device tokens. Device tokens
are revocable and cannot pair further devices or manage sessions,
devices or passkeys. The desktop app keeps the token in the system
keychain, or, where none is available, in a file only the user can read
(and says so).

The desktop web view may only show the app itself. Links in mail open in
the system browser; `mailto:` links open the compose window.

## Rendering email

The server sanitizes every HTML message with an allowlist (ammonia): no
scripts, frames, forms, embedded objects, `base`, `meta`, SVG, MathML or
`<style>` blocks, no event handlers or `javascript:` URLs; inline styles
are filtered per property. Every resource URL is rewritten: `cid:` to a
signed part URL, remote images to a signed proxy URL or a placeholder.
Links open in a new window without opener or referrer.

The message is shown in an `<iframe sandbox>` without `allow-scripts`.
Its document has its own Content Security Policy:

```
default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline';
script-src 'none'; form-action 'none'; base-uri 'none'; frame-ancestors 'self'
```

The app itself runs under
`default-src 'self'; script-src 'self'; img-src 'self' data:; frame-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'`.
Inline images (`cid:`) load through signed URLs valid for 10 minutes.
Attachments download with `Content-Disposition: attachment` and
`nosniff`. Every response carries `Referrer-Policy: no-referrer`.

### Remote images

Remote images are blocked by default, so opening a message does not tell
the sender. **Show images** loads them through the server's image proxy,
never directly from the browser. **Always show from** a sender keeps
loading that sender's images. The proxy:

- fetches only URLs the server signed itself, over `http`/`https` on ports
  80 and 443;
- resolves each host once, accepts only global unicast addresses (no
  loopback, private, link-local, CGNAT, documentation, multicast or
  IPv4-in-IPv6 ranges) and connects to the checked address;
- follows at most three redirects, each checked the same way;
- gives up after 5 seconds or 5 MB, requires an `image/*` type and an
  image signature, and sends no cookies or `Referer`.

## Web Push

Push subscriptions belong to a browser session and end with it.
Endpoints must be `https` and pass the same address check as the image
proxy; redirects are not followed. Payloads are encrypted to the device
(RFC 8291) and padded, so the push service (Google, Apple, Mozilla) sees
only when a notification is sent, not its content or its exact length.

A notification contains the sender's name (or address) and the subject,
cut to 64 and 120 characters. Turn off **Show sender and subject** in Settings →
Notifications to send only "New mail"; sender and subject then never
leave the server. See [pwa.md](pwa.md).
