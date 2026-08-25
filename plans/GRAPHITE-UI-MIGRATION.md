# Graphite UI Migration — bringing the client onto design system v3.3

> **Status:** plan only, nothing implemented.
> **Design source:** `mello-wip/m3llo-design-system.html` (v3.3) and `mello-wip/m3llo-crew-feed-v15.html`.
> **Scope:** restyle what exists. No new panels, no new features. Layout may move
> (the control bar is deliberately regrouped); information does not change.
> **Specs to touch on implementation:** `01-CLIENT.md` §5 (Theme System).

---

## 1. What the design system asks for

| | Today | Target |
|---|---|---|
| Ground | `#181818` / `#202020` / `#222226` | graphite `#121214` / `#17171A` / `#1C1C20` / `#26262B` |
| Accent | `#FF1E56` for *everything* important | **white** = action, **`#FF453A`** = state (the mark, live, rec, unread) |
| Identity | saturated blue/purple/teal/amber/pink | muted `#6E8FB8` `#8E7FB0` `#5FA396` `#B39158` `#B07793` |
| Shape | `border-radius` 6/10/16/20px | one **cut corner** per card, 4 variants; people = octagon; crews/games = 2-corner tile |
| Type | Inter + JetBrains Mono | **Oxanium** (names it, or it is big) + **Barlow** (everything else, two registers) |
| Content | untouched | game footage keeps colour at `saturate(.85)`; chrome is greyscale |

---

## 2. What the codebase already gives us

This is a restyle, not a rebuild. The component inventory maps almost 1:1 onto the mockup.

**Leverage**
- `client/ui/theme.slint` is a real token global — **1525 `Theme.*` references** across 30
  `.slint` files. Retokenising it moves most of the app in one edit.
- The theme already carries backward-compat aliases, so tokens can be renamed without a
  big-bang rename of call sites.
- Fonts are embedded TTFs imported at the top of `theme.slint`. Adding Oxanium and Barlow
  uses the same mechanism.
- The UI is already label-heavy: **217 `Theme.font-mono` vs 38 `Theme.font-sans`**, which is
  exactly the register split the new type system wants.

**Components that already exist for every card in the mockup**

| Mockup | Slint |
|---|---|
| You-bar | `panels/you_strip.slint` → `YouStrip`, `FormPip`, `StatCell` |
| Live / hero clip | `crew_feed.slint` → `HeroClipCard`, `ClipPlayRing`, `DotWaveform` |
| Clip card | `ClipCard` |
| Weekly recap | `RecapCard`, `RecapAvatar` |
| Session | `SessionCard`, `GameSessionCard`, `GameRollupCard`, `panels/session_preview_card.slint` |
| Catch-up | `CatchupCard`, `crew_panel.slint` → `CatchupStoryCard` |
| Empty states | 9 × `Skeleton*Card` |
| Crew rail | `crew_panel.slint` → `ActiveCrewCard`, `CompactCrewCard`, `VcMemberRow` |
| Control bar | `panels/control_bar.slint` → `ControlBar`, `ActionButton` |
| Source picker | `panels/stream_source_picker.slint` (already exists — becomes the caret menu) |
| Chat | `panels/chat_panel.slint` |

---

## 3. The four real obstacles

### 3.1 488 hardcoded hex literals in 29 files
`grep -rno "#[0-9A-Fa-f]\{6\}" client/ui --include=*.slint | wc -l` → **488**.

These bypass `Theme` entirely. Retokenising without sweeping these leaves the app
half-migrated — green-cast panels and pink accents surviving inside otherwise graphite
screens. **This is the largest single source of "it still looks wrong" risk.**

### 3.2 Slint has no `clip-path`
The cut corner is the signature of the system and there is **no `Path` usage anywhere in the
codebase today**. Slint 1.17 does have `Path` with `fill`, `stroke`, `stroke-width` and
`MoveTo`/`LineTo`/`Close` children, so the shape is achievable, but it is new ground:

- a card = one `Path` for the 1px keyline + one `Path` inset by 1px for the fill,
  mirroring how the HTML `::before`/`::after` pair works;
- content that must bleed to the edge (thumbnails) needs either a clipped `Rectangle`
  plus a triangle `Path` in the panel colour over the notch, or a `Path`-shaped fill
  behind an inset image;
- 346 `border-radius` occurrences decide, case by case, whether they become a cut, stay
  rounded (dots, play discs, inputs) or go square.

**This needs a spike before anything else is scheduled.**

### 3.3 Light mode exists, graphite does not have one
`Theme` is written as `dark ? x : y` throughout, and the control bar menu has a
**Light Mode / Dark Mode** item (`control_bar.slint:315`). The new system is dark-only.
Either a graphite-light palette gets designed, or the toggle comes out. **Decision needed.**

### 3.4 `Theme.accent` means two different things today
`#FF1E56` is used for both "press this" and "something is happening", across 23 files.
The new system splits those. Every `Theme.accent` site must be triaged:

- **action** (primary buttons, active channel, focus) → white
- **state** (live, recording, clip, unread, leave) → `#FF453A`

This is the semantic half of the work and cannot be done by find-and-replace.

---

## 4. Order of work

Each step is independently shippable and independently verifiable on screen.

### Step A — Retokenise `theme.slint`
- graphite ramp, white accent + `on-accent`, m3llo red, muted identity set;
- new shape tokens: `cut: 10px`, `notch: 16px`, plus the four corner variants;
- new type tokens: `font-display: "Oxanium"`, `font-body: "Barlow"`, label register
  (`label-weight: 600`, `label-track: 0.16em`), and `font-mono` aliased to Barlow so the
  217 existing call sites keep working;
- add `Oxanium-SemiBold/Bold` and `Barlow-Light/SemiBold` TTFs to `client/ui/fonts/`
  (both SIL OFL). Keep the weight count minimal — the client has a <100MB install budget.
- keep every old token name as an alias. Nothing else changes in this step.

*Verification: app still builds and runs; colours change globally; nothing moves.*

### Step B — Sweep the 488 literals
Mechanical, file by file, mapping each literal to a token. Do it before the shape work so
that later diffs are about geometry, not colour.

### Step C — `CutCard` spike, then the shape primitives
Build in `client/ui/components/`:
- `CutCard` — fill + 1px keyline, `corner` property (`tr` default, `tl`, `br`, `bl`),
  optional accent keyline;
- `CutButton` — top-left + bottom-right, the standard button silhouette;
- `CutTile` — 2-corner, for crew and game tiles;
- `OctAvatar` — 4-corner, for people.

Prove it in a preview file (the `previews/` pattern already exists) at several sizes
before touching any panel.

### Step D — Roll out, surface by surface
In this order, smallest and most visible first:

1. **ControlBar** — also the regroup: left = avatar, name, `● General`, mic, headset,
   divider, leave; centre = Now playing pill + STREAM + caret; right = CLIP 30s + settings.
   `StreamSourcePicker` becomes the caret menu.
2. **CrewPanel** — active crew card with the white keyline, channel rows, other-crew cards.
3. **YouStrip** — smallest surface, already closest to the mockup.
4. **CrewFeed** — the big one (3112 lines, ~20 card components): fixed 232px grid rows,
   cut corners, mono→Barlow label register, tabular figures on every stacked number.
5. **ChatPanel**.
6. **Modals and the rest** — settings, onboarding, sign-in, discover, debug. These can lag
   a release behind without looking broken, because they are mostly text on `surface`.

### Step E — Motion (optional, last)
The travelling edge light on a live card is a fragment shader in Slint: distance along the
border path minus `fract(time / lap)`, `smoothstep` for the head. Only on a card that is
actually live. Nothing else animates.

---

## 5. Verification

- `./scripts/check.sh` after each step (`CI=true` is mandatory, the script sets it).
- Visual passes are **not** verifiable by compiling — each surface needs a look on screen
  via `.\client-prod.ps1`.
- The existing 57 flow tests assert structurally (component type names), not on colour or
  font, so a restyle should not break them. Watch for tests that assert on fixed geometry.

---

## 6. Open decisions

1. **Light mode** — design a graphite-light palette, or remove the toggle?
2. **STREAM button** — stays white (action), or red like prod is today?
3. **Cut corners** — every card, or only the feed and the rail, leaving modals rounded?
4. **Branching** — one branch per surface (A, B, C separately reviewable), or one long-lived
   `feat/graphite-ui` branch?
