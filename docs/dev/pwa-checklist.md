# PWA and Web Push checklist (iOS and Android)

CI covers installability, the service worker, push delivery and the phone
layout in Chromium and WebKit (`e2e/pwa.spec.ts`, `e2e/mobile.spec.ts`).
Real push services, lock screens, Home Screen installs and touch hardware
have no such harness, so before a release the owner runs this list on real
devices and signs it off below.

You need:

- a Tagmail server of the commit under test behind `https` with a public
  name (push services and iOS refuse plain `http` and IP addresses);
- an IMAP account on it whose inbox you can send to from another client;
- an iPhone or iPad on iOS/iPadOS 16.4 or later (Safari);
- an Android phone with current Chrome.

Run every section on both devices. Tick a box only if it held on both;
otherwise note the device and what happened under the section.

## 1. Install and launch

- [ ] Android: menu → **Install app** installs Tagmail; the icon on the
      Home screen is the Tagmail icon, not a screenshot or a letter.
- [ ] iOS: Share → **Add to Home Screen** offers the name "Tagmail" and
      the Tagmail icon (no black or transparent background).
- [ ] Launching from the icon opens `/inbox` standalone: no browser address
      bar or tab strip.
- [ ] Sign in works in the installed app; after closing it from the app
      switcher and launching again, you are still signed in.

## 2. Phone layout and safe areas

- [ ] Portrait and landscape: nothing sits under the notch, the Dynamic
      Island, the status bar or the home indicator; the list scrolls to its
      last row above the home indicator.
- [ ] The menu button opens the drawer with folders and labels; tapping a
      folder or outside the drawer closes it.
- [ ] Tapping a row opens the thread full screen; **Back** (and the Android
      back gesture) returns to the list at the same scroll position.
- [ ] Compose opens full screen; the on-screen keyboard does not cover the
      text you are typing or the **Send** button.
- [ ] Buttons and rows are easy to hit with a thumb; no tap triggers the
      neighbouring control.

## 3. Swipe

- [ ] Swipe a row left: it is archived and an undo toast appears; **Undo**
      puts it back in place.
- [ ] Swipe a row right: it moves to Trash with an undo toast; **Undo** puts
      it back.
- [ ] A short swipe snaps back and does nothing.
- [ ] Scrolling the list vertically never archives or deletes a row.
- [ ] In Trash, a right swipe does not delete anything permanently.

## 4. Offline

- [ ] With airplane mode on, launching the installed app shows the app and
      "You're offline. Mail will load when you're back online."
- [ ] Turning airplane mode off loads mail again without reinstalling or
      signing in.

## 5. Notifications

- [ ] iOS in a Safari **tab** (not installed): Settings → Notifications
      says to add Tagmail to the Home Screen first.
- [ ] In the installed app: Settings → Notifications → **Enable
      notifications** asks for permission once; after allowing, the page
      shows the enabled state.
- [ ] **Send test notification** arrives within a few seconds.
- [ ] With the device locked, send a mail to the inbox from another
      client: a notification with the sender as title and the subject as
      body appears on the lock screen within a minute.
- [ ] Tapping it opens the app on that thread (after unlocking).
- [ ] Three mails sent within a few seconds give one notification
      "3 new messages"; tapping it opens the inbox.
- [ ] With **Show sender and subject** off, a new mail shows "New mail" and
      no sender or subject.
- [ ] A mail filed straight into another folder (a server-side filter)
      gives no notification.
- [ ] The device appears in the list of subscribed devices on the other
      device, and **Remove** there stops its notifications.
- [ ] Denying permission (or blocking it in the system settings) shows
      "Blocked in browser settings"; nothing else breaks.

## 6. Session end

- [ ] **Sign out** in the installed app; a new mail to the inbox gives no
      notification on that device.
- [ ] Sign in again and enable notifications: they arrive again.
- [ ] Revoke the device's session in Settings → Devices & sessions on
      another device; a new mail gives no notification on the revoked
      device. Signing in again there brings them back without enabling them
      again.

Notifications also stop when the session expires (30 days without opening
the app, 90 days after signing in). This is accepted and documented in
`docs/pwa.md`; it is not part of this run.

## Sign-off

Commit: ____________ iOS device / version: ____________
Android device / Chrome version: ____________

Owner: ____________ Date: ____________ Result: pass / fail (notes above)
