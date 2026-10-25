# SEALED-REVEAL

> **Component:** Crew Feed, Head2head, Weekly Recap
> **Status:** plan, in work on `feat/next-gen-feed`
> **Depends on:** `specs/features/HEAD2HEAD.md`, `specs/19-FEED-CURATION-PERSONAL-STATS.md`
> **Origin:** the fx lab (a private scratch project). Bob approved its look and motion.

A belt that moved, and the weekly recap, arrive sealed in the feed. A click opens
the card in the centre of the feed. A second click sends it back to its slot, open.

## 1. What is built

| Part | File | Note |
|---|---|---|
| Clock, easing, hash | `client/ui/components/sealed.slint` | One `Clock` global. Manual mode for tests. |
| Sealed chip, sealed recap | `client/ui/components/sealed.slint` | Code cells are one `Path` each. |
| Centre view | `client/ui/panels/sealed_stage.slint` | Made with `if` when a card leaves its slot. |
| Belt data for the UI | `client/src/sealed.rs` | Key, seed, code commands, count-up value. |
| Opened state | `client/src/settings.rs` | `sealed_opened`, by crew. Local only. |
| State port | `client/src/e2e_state.rs` | `sealed {belts, recap, open_center}`. |

Sealed cards are:

- the chip of a belt that moved, in the head2head pair rows, the versus hero, the
  weekly recap and the daily recap;
- the weekly recap card.

- the own chips of the solo head2head card of a new crew (section 9).

The belt hero (frame E) and the belt push keep their copy.

## 2. Sealed looks

All belts look the same sealed. Nothing on the chip tells which belt it is.

### 2.1 Sealed chip

Same silhouette and size as `TitleChip` (button shape, top-left and bottom-right cut).

| Part | Value | Token |
|---|---|---|
| Fill | `#161618` | `Theme.surface` |
| Keyline | rest | `Theme.graphic-med` |
| Keyline | hover, and lift 3 px in 120 ms | new `Theme.action-line-hover` `#FFFFFFB0` |
| Cut | 7 px, 5 px for compact and dense | as `TitleChip` |
| Cell | 4 px square, 5 px pitch | `Theme.action-quiet` |
| Grid | 15 columns, 6 rows, stride 15. The chip shows the columns and rows that fit. | |
| Padding | 12 px, 10 px for compact and dense | |
| Key | square, 12 px (10 px compact), 9 px after the code, centred | `Theme.live` |

The cell is on when `hash(i * 1.37 + seed * 101.3 + 0.17) > 0.45`, with
`i = row * 15 + col`. The seed is a stable hash of `title_id` and `moved_at_ms`.
One belt move has one seed, so the code is the same in every slot and in the centre.
Rust builds the cells and the `Path` commands once for each belt (`sealed.rs`).

In a recap row, the new holder is a muted octagon with "?". The previous holder
stays at 0.4 opacity.

### 2.2 Sealed recap

Same shape as the recap card (`CutShape` corner `bl`). Sizes are relative to the card
size `w` by `h`.

| Part | Value | Token |
|---|---|---|
| Fill | as the chip | `Theme.surface` |
| Keyline | rest, hover (lift 4 px in 140 ms) | `Theme.graphic-med`, `Theme.action-line-hover` |
| Text | `WEEKLY RECAP` label 11 px at (28, 26). Then the week title, Oxanium 700 40 px | `Theme.text-secondary`, `Theme.text-primary` |
| Grid | 13 columns by 20 rows. Origin `(w * 14/160, h * 8/248)`. Pitch `(w * 11/160, h * 12/248)` | |
| Cell | `w * 5/160` square. No cells above `h * 70/248` | |
| Cell value | `v = hash(i * 1.37 + seed * 57.1 + 0.31)`. `v > 0.93` bright. `v > 0.76` dim | `Theme.action-quiet`, `Theme.card-border` |
| Key | `w * 9/160` square, on cell column 10, row 15 | `Theme.live` |

The seed is a stable hash of the week start. Two `Path`s draw the field, one for each
level of the cells.

## 3. The opening and the return

All times are from the click. One `Clock` drives all of it.

| Time (ms) | What happens |
|---|---|
| 0 to 520 | Flight from the slot to the centre. Position and scale on ease-in-out-cubic. Tilt `sin(k * 180deg) * -7deg`. Scrim `Theme.media-ground` at `0.8 * k`. |
| 520 to 860 | Charge. Shake (x up to 5 px, rotation up to 1.6 deg). A white glow builds. The code cells clear outward from the key. The key grows 30 to 35 %. |
| 860 to 1200 | Flip. `transform-scale-x = abs(cos(flip * 180deg))`. A dark overlay at `(1 - squash) * 0.7`. The face replaces the back at 1030. |
| 1030 to 1260 | White flash `(1 - span)^2`, peak 0.75. Back light disc. |
| 1030 to 1750 | Burst in the hue of the new holder: shock ring, 40 streaks, 30 sparks. 14 rays come in at 980 to 1250 and settle to a low glow at 1600 to 2600. |
| 1150 to 2050 | The value counts up (ease-out-cubic). |
| 1250 to 1650 | The holder avatar pops in (ease-out-back from 0.4). "took it from" fades in at 1500 to 1900. |
| from 1250 | Rest. Embers on the edge of the big belt (80 particles). A click on the scrim closes. |
| Close, 0 to 460 | The card flies back. The slot fades in from 340. The card fades out from 380 to 460. |

The glow is white until the flip. Then it takes the hue of the new holder. The
colour must not name the holder early.

Sizes in the centre:

- **Big belt:** 800 by 400, button shape, 36 px cut. The back is the chip at 8 times:
  15 by 6 cells, 32 px cells, 40 px pitch. The face has the label
  `HEAD2HEAD · BELT MOVED`, the icon and title (Oxanium 700 50 px), the value
  (Oxanium 700 120 px, unit in Barlow 300 34 px), the stat sentence (Barlow 300 18 px),
  and on the right the holder (avatar 112 px, name Oxanium 600 34 px) and
  "took it from" with the previous holder (avatar 22 px).
- **Recap:** the recap card, scaled by `min(1.1, (page height - 70 px) / h / 1.06)`.
  The back clears with radius `clear * 470 px`, 80 px soft edge. The numbers count up.
  The revealed card has a control "See the week". It opens `recap_opened.slint`.

The count-up has three kinds: a count with thousands (one decimal below 100 hours),
hours and minutes (`4 h 05`), and a clock (`03:22`). `sealed.rs` sends the number and
the kind. The display string does not count up.

The stat sentences are the Stat column of HEAD2HEAD.md section 2, in `sealed.rs`.

## 4. State

- The key of a belt move is `title_id:moved_at_ms`. The key of a recap is
  `recap:<week_start_ms>`.
- The opened keys are in the client settings, by crew: `sealed_opened`. Like
  `feed_seen_ms`, they stay on this device. A server copy comes later.
- A card is sealed when its key is not opened and the move is under 7 days old. An
  older card opens silently on the next feed load: no animation, no stored key.
  A stored key older than 15 days is dropped when a card opens. A recap stays sealed
  for 7 days after its week ends, and its key holds the week start.
- A click on any slot of a belt opens the belt in every slot. While the belt is in the
  centre, its other slots stay sealed. They open when the card lands.
- The key is stored when the card leaves its slot. The feed cards are built again
  when the card lands.
- Opening a belt clears the unread dot on its pair rows. A pair row has the dot only
  for a recent move that is not opened.
- The versus hero keeps its tally. Its moved chip is sealed like any chip.
- The recap opens in the centre. The full-screen recap (`recap_opened.slint`) is
  reached from the revealed recap card by "See the week".
- A belt that the viewer does not see sealed (a chip of a title that has not moved)
  has an empty key and is never sealed.

State port, `sealed`:

| Field | Meaning |
|---|---|
| `belts` | Number of different sealed belt keys in the feed. Own chips are not in it |
| `own` | Number of different sealed own chips (section 9) |
| `recap` | The weekly recap is sealed |
| `open_center` | `belt`, `recap` or `none` |

## 5. Performance rules

Measured in the fx lab on an M3 Max at 120 Hz with Skia.

- Never animate the blur, size or colour of a drop shadow. Slint draws a new offscreen
  surface for each frame, and memory grows by about 400 MB. A glow is a fixed shadow
  with an animated `opacity`, or a radial gradient. Particles use a radial gradient.
- Read the clock only while an effect runs. A binding that reads `animation-tick()`
  redraws the window each frame. Sealed cards and open cards read no clock.
- Make the centre view with `if` when a card leaves its slot. A hidden view that
  exists costs a frame cost.
- Draw each code matrix as one `Path` with commands built in Rust. The centre view
  uses one `Rectangle` for each cell for the 340 ms of the charge only, to clear the
  cells outward from the key.
- Streaks are plain rectangles. Radii are only for dots, discs and pips.
- A `changed` handler runs later, in no fixed order. Two state changes in one step
  can run in the wrong order. Open and close use separate steps.

Targets:

| Measure | Target |
|---|---|
| Feed with sealed cards, idle | 0 redraws |
| RSS at rest with sealed cards | under 100 MB |
| Memory left behind by one reveal | under 40 MB |

If a reveal leaves more than 40 MB, the particle counts go down.

## 6. Accessibility

Every `TouchArea` has `accessible-role` and `accessible-label`.

| Control | Role | Label |
|---|---|---|
| Sealed chip | button | `Sealed belt, open it` |
| Sealed recap | button | `Sealed recap, open it` |
| Scrim of the centre view | button | `Close` |
| Recap control | button | `See the week` |

The centre view has no other control. The a11y lint (`a11y_lint.rs`) must pass.

## 7. Tests

Harness (`client/src/flow_tests.rs`, `client/src/sealed.rs`):

- A belt that moved and is not opened is sealed.
- Open sets the state and seals nothing in any slot of that belt.
- The unread dot clears when the belt opens.
- A move older than 7 days is not sealed.
- The recap is sealed by week, and opening it does not open another week.
- The centre view is absent when nothing is open.
- A frame at a fixed clock time renders (manual `Clock`).
- The sealed state survives a restart (settings).
- The code of a belt is the same in every slot (same seed, same commands).

Journey `qa/journeys/feed-sealed-reveal.ts` (FEED-10): bob with a seeded crew and
a belt that moved. `sealed.belts` is 1. Click the sealed chip by label. Wait for
`open_center` `belt`. Checkpoint at once and after 1.3 s (the face). Click Close.
The slot is open and the dot is clear.

Previews and renders: `PreviewFeedSealed` (stage D with sealed belts and recap) and
`PreviewRevealFrame` (the centre view at 1400 ms), rendered with `slint-viewer`.

## 8. Measured

Release build with the e2e feature, Skia, macOS arm64, one reveal of a belt in a crew of
five (`feed-sealed-reveal` flow, RSS from `ps`).

| Moment | RSS |
|---|---|
| Feed with no sealed card | 184 MB |
| Feed at rest with a sealed card | 186 MB |
| Card at rest in the centre | 219 MB |
| After the card returned, 4 s later | 224 MB |

- A sealed card costs 2 MB and no redraw.
- One reveal leaves 38 MB behind. This is under the limit of 40 MB, so the particle
  counts stay.
- The whole app is over the 100 MB target at rest, with and without a sealed card. The
  idle scenario of the perf harness measures the core without the UI and shows no
  regression.

The `slint-viewer` renders (`PreviewFeedSealed`, `PreviewRevealFrame`) use the software
renderer. It draws no radial gradient on a `Path` and no transform, so the rays are flat
and the card does not tilt or flip in those two files. The journey checkpoints show the
Skia frames.

## 9. Day 0: the own chips of a new crew

A crew of one has no belt that moved. The solo head2head card (HEAD2HEAD.md section 5)
shows the viewer's own values as chips. So that the first user gets the reveal too,
those chips arrive sealed.

| Part | Rule |
|---|---|
| Chip | A `SealedChip` in the place of each of the four own chips. Same back as a belt chip. Label `Sealed belt, open it` |
| Key | `own:<title_id>:<crew_id>`. No value and no time in it |
| Seed | A stable hash of `own`, the title id and the crew id |
| Opening | A click sends the chip to the centre with the same flight, charge, flip and burst. The hue is the viewer's own |
| Face | Label `HEAD2HEAD  ·  YOUR TITLE`, the icon and title, the counted-up value, the stat sentence, the viewer's avatar and `You`. No "took it from" |
| Return | The open chip lands in its slot with the value |
| Stored | The key goes in `sealed_opened` when the chip leaves its slot |
| Silent open | The chips are sealed for 7 days from the first time they showed (`own_seal_since`, by crew, local). After that they open with no animation and no stored key |
| Later days | A key that was opened once stays open when the value changes, because the key has no value |

- Each own chip opens alone. The other three wait in their slots while one is in the centre.
- The sealed solo card keeps the headline "Go head2head with your crew", the four open seats and the invite.
- A crew of two or more has no own chips. Its pair chips follow sections 2 to 4.
- State port: `sealed.own`.
- Harness tests: a new crew seals all own chips, opening one opens only that one, a reopened crew keeps opened chips open when the value changes, and a crew older than 7 days has them open (`flow_tests/sealed.rs`).
- Journey: `feed-solo-head2head` opens one own chip, checkpoints the face and closes it.
