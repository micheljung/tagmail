# Keyboard shortcuts and search

Press `?` in the app to show the shortcut overlay; press it again to close
it.

Shortcuts work like Gmail's: single keys, and two-key sequences starting
with `g` (the second key within 1.5 seconds). They are ignored while you
type in a text field and while Ctrl, Alt or Cmd is held. `Escape` leaves a
text field. While a dialog is open, only `?` works.

## Navigate

| Keys | Action |
|------|--------|
| `j` | Older conversation |
| `k` | Newer conversation |
| `o`, `Enter` | Open conversation |
| `u` | Back to the list |
| `/` | Search mail |
| `g` then `i` | Go to Inbox |
| `g` then `l` | Go to label |
| `?` | Keyboard shortcuts |
| `Escape` | Leave the focused field |

In the conversation list, `↑`, `↓`, `Page Up`, `Page Down`, `Home` and
`End` move the selection as well.

## Act

| Keys | Action |
|------|--------|
| `x` | Select conversation |
| `e` | Archive |
| `l` | Label as… |
| `v` | Move to… |
| `#` | Delete |
| `s` | Star or unstar |
| `Shift`+`i` | Mark as read |
| `Shift`+`u` | Mark as unread |

## Write

| Keys | Action |
|------|--------|
| `c` | Compose |
| `r` | Reply |
| `a` | Reply all |
| `f` | Forward |
| `Ctrl`+`Enter` (`Cmd`+`Enter` on macOS) | Send, in the compose form |

## Search operators

Terms next to each other must all match. `OR` (uppercase) matches either
side and binds tighter than the implicit AND: `a OR b c` is
`(a OR b) c`. `-term` or `NOT term` negates, parentheses group, and
`"quoted words"` match as a phrase. A trailing `*` on a word or a
`subject:` value matches by prefix.

| Operator | Matches |
|----------|---------|
| `from:` | Sender name or address |
| `to:` | To recipient name or address |
| `cc:` | Cc recipient name or address |
| `subject:` | Words in the subject |
| `label:` | A label by name |
| `in:` | A mailbox by name, or `in:anywhere` |
| `is:unread`, `is:read`, `is:starred` | Status |
| `has:attachment` | Messages with attachments |
| `before:`, `after:` | Date as `YYYY/M/D` or `YYYY-M-D` (UTC) |

Example: `invoice from:billing@example.com after:2026/01/01 -is:read`.
