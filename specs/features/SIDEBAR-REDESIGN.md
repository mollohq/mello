# SIDEBAR REDESIGN

> **Component:** Sidebar Layout, Crew Navigation  
> **Version:** 1.0  
> **Status:** Design  
> **Related:** CLIPS.md, 11-PRESENCE-CREW-STATE.md, 13-VOICE-CHANNELS.md  
> **Mockups:** m3llo-crew-feed-mockup-v4.html

---

## 1. What This Is

The sidebar redesign accompanies the crew feed moving to center stage. With game activity, sessions, catch-up, and clips now in the center view, the sidebar becomes focused: voice channels for the active crew, and glanceable status for other crews.

---

## 2. Why This Changes

Previously the sidebar showed game activity (PLAYING section), catch-up info, and detailed member lists for the active crew. With the crew feed occupying the center, all of that information is now displayed with more space and richer presentation in the bento grid. Repeating it in the sidebar wastes space and creates visual noise.

The sidebar has two jobs now:

1. **Active crew:** Where do I talk? (voice channels)
2. **Other crews:** Should I switch? (glanceable FOMO)

---

## 3. Active Crew Card

The active crew card shows the crew header and voice channels. Nothing else.

### 3.1 Layout

```
┌─ ACTIVE CREW ────────────────────┐
│                                   │
│  [icon] CREW NAME                 │
│         SFU  5 / 10          ●    │
│                                   │
│  🎙 General                    3  │
│     [os] ostkatt          |||     │
│     [Fa] FaiL                     │
│     [b0] b0bben                   │
│                                   │
│  🎙 Ranked grind               2  │
│     [w1] win_dev_1        |||     │
│     [tt] titovicka                │
│                                   │
│  🎙 Chill                      0  │
│  🎙 Movie night                0  │
│                                   │
└───────────────────────────────────┘
```

### 3.2 Voice Channels

- Channels with members show the channel name, member count (green), and an expanded member list below with avatars and speaking indicators
- Empty channels collapse to a single line: channel name and "0" in muted gray
- Channel names are click targets for joining
- Speaking indicators (animated bars in brand red) show next to members who are actively talking

### 3.3 Max Height Constraint

The active crew card has a max-height of 50% of the sidebar. If the member list exceeds this (many channels, many members), the card scrolls internally. This guarantees the bottom half of the sidebar is always available for non-active crews.

This is the hard rule. Regardless of how many voice channels or members exist, non-active crews are never pushed off screen.

### 3.4 What Is NOT in the Active Crew Card

- No game activity (PLAYING section) -- moved to crew feed "now playing" and "recent games" cards
- No catch-up text -- moved to crew feed catch-up cards
- No invite button -- moved to crew feed invite CTA card during cold start
- No chat preview -- chat has its own panel on the right
- No member list outside of voice channels -- online/offline member lists are not shown

---

## 4. Non-Active Crew Cards

All non-active crews use the same card format regardless of activity level. No visual distinction between "active" and "idle" crews. The difference is purely informational: active crews have an activity line, idle crews don't.

### 4.1 Layout

Every non-active crew card has the same structure and height:

```
┌──────────────────────────────────┐
│ [icon] Crew Name        ✂ 4     │
│                                  │
│ ● vex_r streaming Valorant       │
│                                  │
│ vex_r  yo who has the stash...   │
│ lune   check the drop box, i... │
└──────────────────────────────────┘
```

### 4.2 Components

**Row 1: Header**
- Crew icon (22px, rounded rectangle, crew color at 20% opacity, dimmed)
- Crew name (12px, #777, brightens to #aaa on hover)
- Clips FOMO badge (right-aligned, only shown when crew has new clips): scissors icon + count in a small pill, brand red at low opacity (#EB4D5F18 background, #EB4D5F88 text)

**Row 2: Activity line (optional)**
- Only shown when something is happening: streaming, voice activity, etc.
- Small colored dot (4px) indicating activity type: red for streaming, green for voice
- Activity description text (#555, 10px)
- For voice activity: small inline avatar dots (14px, 60% opacity) showing who's in voice, for instant recognition

Priority for what to show (pick one):
1. Someone streaming (highest priority)
2. People in voice (show count + avatar dots)
3. Nothing happening (skip this row entirely)

**Row 3-4: Chat preview (always shown)**
- Last 2 chat messages, always displayed regardless of activity
- Author name in bold (#555), message text (#444), truncated with ellipsis
- 10px font size, single line per message
- On hover, text brightens slightly

### 4.3 Visual Treatment

Non-active crew cards are intentionally dimmed compared to the active crew:

- Background: #1e1e22 (slightly darker than regular cards)
- Border: 1px solid #2a2a2e (subtle, not competing with active crew)
- All text and icons at reduced brightness
- On hover: background lightens to #232328, border to #363636, text brightens

The dimming creates visual hierarchy. The active crew card (with its brand-red border tint) clearly dominates. Non-active crews are present but recede.

### 4.4 Clips FOMO Badge

The clips badge appears on non-active crew cards when the crew has new clips since the user last viewed that crew's feed. It's a small pill with the scissors icon and a count:

```
✂ 4
```

Background: #EB4D5F18 (brand red at very low opacity)
Border: 1px solid #EB4D5F22
Text: #EB4D5F88
Font: 9px, bold

The badge disappears when the user switches to that crew (marking clips as seen). If the crew has no new clips, no badge is shown. The absence of the badge on most crews makes it pop more when it IS there.

### 4.5 Voice Avatar Dots

When the activity line shows voice activity, small inline avatar dots appear after the text:

```
● 3 in voice  [zr] [nv] [ax]
```

Avatars are 14px rounded rectangles at 60% opacity, using the same avatar colors as the full member list. This enables instant recognition: "oh, nova and zeroX are on" without needing to read names or switch crews.

On hover, avatars brighten to 80% opacity.

---

## 5. Sidebar Layout

The full sidebar layout from top to bottom:

```
┌─────────────────────────────┐
│ SECTION LABEL: Your crews    │
│                              │
│ ┌──────────────────────────┐ │
│ │ ACTIVE CREW              │ │
│ │ (voice channels, max 50%)│ │
│ └──────────────────────────┘ │
│                              │
│ SECTION LABEL: Other crews   │
│                              │
│ ┌──────────────────────────┐ │
│ │ Non-active crew 1        │ │
│ └──────────────────────────┘ │
│ ┌──────────────────────────┐ │
│ │ Non-active crew 2        │ │
│ └──────────────────────────┘ │
│ ┌──────────────────────────┐ │
│ │ Non-active crew 3        │ │
│ └──────────────────────────┘ │
│ ...scrollable if needed...   │
│                              │
│ ─────────────────────────── │
│ 🔍 Browse crews              │
│ + New crew                   │
└─────────────────────────────┘
```

### 5.1 Space Budget (1080p, ~900px usable sidebar height)

- Section label "Your crews": ~20px
- Active crew card (max 50%): up to ~450px
- Section label "Other crews": ~20px
- Non-active crews: remaining space (~400px+), scrollable
- Browse/New crew actions: ~60px fixed at bottom

Each non-active crew card is approximately 80-90px tall (header + optional activity line + 2 chat lines). This comfortably fits 4-5 non-active crews without scrolling in typical cases.

### 5.2 Scrolling

If the user has more non-active crews than fit, the "Other crews" section scrolls independently. The active crew card and the bottom actions (Browse/New crew) stay fixed. Only the non-active crew list scrolls.

---

## 6. Avatars

All avatars throughout the sidebar use rounded rectangle shape (border-radius: 4px), not circles. This is consistent with m3llo's design language across the entire app.

---

## 7. Interaction

- Clicking a voice channel in the active crew card joins that channel
- Clicking a non-active crew card switches to that crew (it becomes the active crew, the previous active crew becomes a non-active card)
- The clips FOMO badge on a non-active crew clears when the user switches to that crew
- Browse crews and New crew open their respective flows

---

## 8. What This Does NOT Cover

- The crew feed center view (see CLIPS.md)
- The chat panel on the right (unchanged)
- The bottom bar (unchanged, see CLIPS.md for clip button placement)
- Mobile layout (not applicable, desktop app only)

---

*Companion spec to CLIPS.md. Together they define the new layout where the sidebar handles navigation and the center handles content.*
