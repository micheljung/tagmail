# Desktop release checklist (macOS and Windows)

CI builds and self-tests the desktop app on Linux under Xvfb and checks
notifications (AC 17) and `mailto:` (AC 18) there automatically. macOS
(WKWebView) and Windows (WebView2) have no such harness, so before a release
the owner runs this list on real hardware and signs it off below.

Bundles come from the **Release** workflow (`release.yml`), which builds
them without publishing on pull requests and manual runs: download the
`bundles-universal-apple-darwin` (`.dmg`) and
`bundles-x86_64-pc-windows-msvc` (`.msi`, `-setup.exe`) artifacts of the
commit under test and compare them against the `SHA256SUMS` in its
`release-assets` artifact. The bundles are unsigned (owner decision): on
macOS open the app with right-click → Open, on Windows choose "More info →
Run anyway" in SmartScreen.

You need a Tagmail server reachable from the machine, with an IMAP account
whose inbox you can send to from another client.

## 1. In-webview self-test (header CSP, iframes, events)

The self-test is the Linux CI check run by hand. It needs a debug build with
the `test-hooks` feature, which the release bundles never contain:

```sh
pnpm install --frozen-lockfile && pnpm --filter @tagmail/web run build
cargo build -p tagmail-desktop --features app,test-hooks
TAGMAIL_DESKTOP_SELFTEST=1 TAGMAIL_DESKTOP_DATA_DIR="$(mktemp -d)" target/debug/tagmail-desktop
```

On Windows run the last line in Git Bash, or in PowerShell set the two
variables with `$env:` first. The app opens, runs for a few seconds and
exits; it prints one `DESKTOP_SELFTEST {...}` line.

- [ ] macOS: exit code 0 and `"ok":true`; every entry in `results` is ok
      (`spa_inbox`, `transport`, `spa_csp_blocks_eval`,
      `iframe_height_readable`, `render_blocks_img`, `sandbox_blocks_script`,
      `emit_events`).
- [ ] Windows: the same, and `origin` is `http://app.localhost`.

If a check fails, paste the whole line into the release issue.

## 2. Pairing and sync

Install the bundle (drag `Tagmail.app` to Applications; run the `.msi`).

- [ ] On the web, Devices & sessions → Pair a device; in the app enter the
      server address and code. The app shows "Downloading your mail…" with a
      percentage, then the inbox.
- [ ] The inbox matches the web inbox (count and newest 20 rows).
- [ ] The "This device" page shows the server, Online and a last sync time.
- [ ] Opening a `tagmail://pair?server=…&code=…` link (paste it into the
      browser's address bar) focuses the app with the pairing form filled in.

## 3. Notifications and unread badge (AC 17)

Allow notifications when the OS asks (macOS: System Settings → Notifications
→ Tagmail).

- [ ] With the app window closed to the tray / menu bar, send a mail to the
      inbox from another client. Within a minute a system notification shows
      the sender and subject.
- [ ] Clicking the notification opens the app on that conversation's inbox.
- [ ] Sending four or more mails at once shows one summary notification,
      not four.
- [ ] macOS: the Dock icon badge shows the unread inbox count; reading a
      message lowers it.
- [ ] Windows: the taskbar button shows the unread count overlay; reading a
      message lowers it.
- [ ] Turning off "Show sender and subject in notifications" on the
      "This device" page makes the next notification say only "New mail".

## 4. Default mail app (AC 18)

- [ ] macOS: Mail → Settings → General → Default email reader → Tagmail.
      Windows: Settings → Apps → Default apps → Tagmail → set as default for
      `MAILTO`.
- [ ] With the app **not** running, click
      `mailto:ana@example.com?subject=Hello&body=Line%201%0D%0ALine%202`
      in a browser. The app starts with a compose window: To
      `ana@example.com`, Subject `Hello`, body on two lines.
- [ ] With the app already running, click
      `mailto:a@example.com,b@example.com?cc=c@example.com&attach=/etc/passwd`.
      The running window comes to the front (no second window), compose has
      both To addresses and the Cc, and no attachment.

## 5. Offline

- [ ] Stop the server (or disconnect the network). The app shows the offline
      banner; the inbox and recently read conversations still open.
- [ ] Searching shows "Older results need a connection" above the results.
- [ ] Archive a conversation and send a mail. Both show as unsynced.
- [ ] Bring the server back. Within a minute the unsynced count drops to
      zero, the archive shows on the web, and the mail arrives exactly once.

## 6. Window and startup

- [ ] Closing the window keeps the app in the tray / menu bar (default);
      "Open" in the tray menu brings it back, "Quit" ends it.
- [ ] "Start Tagmail when I sign in" on: after logging out and in, the app
      runs in the tray. Turn it off again.

## Sign-off

| Platform | Version / build | Commit | Date | All checked | Name |
|---|---|---|---|---|---|
| macOS | | | | | |
| Windows | | | | | |
