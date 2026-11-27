# Rendering corpus checklist

Record of the B0 exit gate and the Tagmail-vs-Thunderbird comparison (plan
`tagmail-mobile-experience.md`, Step 4 and AC16).

**Status: the real-corpus rows are pending the user's problem emails.** They
have not arrived, so AC16 and the B1 go/no-go are blocked. The synthetic rows
below exercise the rules only; they never count toward the B1 gate, because
the synthetic `responsive` email flips to scaled by construction.

Original `.eml` files and Thunderbird screenshots go in `private/` (git-ignored).
Only anonymised reconstructions are committed next to this file.

## Columns

- **data-scale**: the `.paper` `data-scale` at 390px (iPhone 13) and 412px (Pixel 7).
- **overflow**: AC13 holds: no page overflow, no element past the frame edge.
- **flipped to scaled**: the source has author `<style>` with `@media` width
  rules, and B0 renders it with `data-scale < 1`.
- **flipped to desktop**: predicted at gate time (author `max-width`
  breakpoint < the B0 natural width); measured with the B1 flag on once B1 exists.
- **line breaks / widths / centering**: Tagmail at 390px vs Thunderbird at a
  matching width, pass or fail per symptom.
- **sign-off**: the user's initials and date.

## Real corpus (pending)

| email                         | data-scale 390 / 412 | overflow | flipped to scaled | flipped to desktop predicted / measured                                                                                         | dominant px (effective 390 / 412) | Thunderbird comparison                                                     | sign-off |
| ----------------------------- | -------------------- | -------- | ----------------- | ------------------------------------------------------------------------------------------------------------------------------- | --------------------------------- | -------------------------------------------------------------------------- | -------- |
| real-1 (booking confirmation) | 0.924 / 0.979        | none     | yes               | no (author `max-width:599px` still matches at the 396px natural width) / no (B1: data-scale 1 / 1, natural width = frame width) | 16 (14.78 / 15.66)                | pending: Thunderbird not available; browser reference render used as proxy | pending  |

Real-corpus n = 1 (measured 2026-11-27, B0 via the render path, Playwright Chromium; the
reference render, the original HTML with remote images blocked and no scaling, fits at both
widths: `scrollWidth` equals the viewport). Screenshots stay in `private/`.

## B1 (author CSS, flag on), measured 2026-11-27

`--render-author-css` on, same harness as the B0 rows. real-1: data-scale 1 at 390 and 412
(was 0.924 / 0.979), no overflow, dominant text 16 px (effective 16 / 16); single-column
layout like the reference render; blocked remote images leave empty boxes where the
reference shows alt text. Synthetic: every corpus email passes AC13 with the flag on
(`mobile.spec.ts`, "email rendering with author CSS (B1)"); `responsive` renders at
data-scale 1 with its columns stacked. Flipped to desktop (measured): none.

## Synthetic corpus (B0, measured 2026-11-27)

Measured in Playwright on `mobile-webkit` (iPhone 13, `.paper` 366px wide) and
`mobile-chromium` (Pixel 7, `.paper` 388px wide). No Thunderbird comparison: these are
not user emails.

| email           | data-scale 390 / 412 | overflow | flipped to scaled | flipped to desktop predicted / measured                                                                    | notes                                                              |
| --------------- | -------------------- | -------- | ----------------- | ---------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| responsive      | 0.61 / 0.646         | none     | yes (synthetic)   | no (`max-width:600px` still matches at the 600px natural width) / no (B1: data-scale 1 / 1, columns stack) | `<style>` and `class` stripped in B0, so the 600px table is scaled |
| fixed-600       | 0.61 / 0.646         | none     | no                | no / n.a.                                                                                                  | columns stay 200px                                                 |
| text            | 1 / 1                | none     | no                | no / n.a.                                                                                                  |                                                                    |
| centered        | 1 / 1                | none     | no                | no / n.a.                                                                                                  | fluid `max-width:480px` card                                       |
| long-token      | 1 / 1                | none     | no                | no / n.a.                                                                                                  | linked 300-char URL wraps (`a{overflow-wrap:anywhere}`)            |
| long-token-bare | 0.152 / 0.15         | none     | no                | no / n.a.                                                                                                  | known gap: a bare token widens its cell                            |

## Device checklist (plan section 6, step 8)

| check     | Android Chrome PWA | iOS Safari standalone |
| --------- | ------------------ | --------------------- |
| _pending_ |                    |                       |
