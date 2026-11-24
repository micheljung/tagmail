# Installing the web app and notifications

The web app can be installed on phones, tablets and desktops like a native
app, and can show a notification when new mail arrives in an inbox.

Both need the server behind `https` (`--public-origin https://…`).
Browsers offer neither on a plain `http` address other than `localhost`.

## Install

- **Android (Chrome):** open Tagmail, then menu → **Install app** (or
  **Add to Home screen**).
- **iPhone and iPad (Safari, iOS/iPadOS 16.4 or later):** Share →
  **Add to Home Screen**, then open Tagmail from the Home Screen icon.
- **Desktop (Chrome, Edge):** the install icon in the address bar.

The installed app keeps its own window. Offline it still opens and shows
"You're offline. Mail will load when you're back online."; mail itself is
not stored on the device. For offline reading, search and compose use the
desktop app.

## Notifications

Turn them on in **Settings → Notifications → Enable notifications**, once
per device. The browser asks for permission at that moment.

- **iPhone and iPad:** notifications work only in the app opened from the
  Home Screen, not in a Safari tab. Install it first (see above).
- **Show sender and subject:** on by default. Turn it off to get "New mail"
  only; sender and subject then never leave the server.
- With several accounts, choose which inboxes notify on each device.
- **Send test notification** checks the whole path to the device.
- **Turn off on this device** stops them on this device; the list below it
  removes other devices.

Tagmail notifies about new unread mail in an inbox only: not about other
folders, mail that is already read, mail moved into the inbox, or the
messages of the first sync of a new account. Several messages within a few
seconds become one notification ("3 new messages").

### When notifications stop

Notifications belong to the browser session they were turned on in. They
stop when you sign out on that device, when the session is revoked in
**Settings → Devices & sessions**, or when the session expires: after 30
days in which the app is not opened, or 90 days after signing in. Opening
the app (for example by tapping a notification) keeps the session alive.
After an expired or revoked session, signing in again on the device turns
them back on; after signing out, enable them again in Settings.

Notifications are sent through the push service of the browser vendor
(Google, Apple, Mozilla). The content is end-to-end encrypted to the device;
the push service sees only when and how large.
