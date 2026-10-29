# MELLO Client Specification

> **Component:** Desktop Client (Slint UI)  
> **Language:** Rust  
> **Status:** Beta Scope  
> **Parent:** [00-ARCHITECTURE.md](./00-ARCHITECTURE.md)

---

## 1. Overview

The Mello client is a native desktop application built with Slint UI in Rust. It provides the user interface for crew management, voice chat, text chat, stream viewing, and onboarding. All application logic lives in `mello-core`; the client is purely a UI shell that sends `Command`s and reacts to `Event`s.

---

## 2. Technology Choices

| Aspect | Choice | Rationale |
|--------|--------|-----------|
| Language | Rust | Memory safety, small binaries, modern ecosystem |
| UI Framework | Slint | Declarative, native performance, <2MB, Apache 2.0 |
| Async Runtime | Tokio | Industry standard for Rust async |
| State Management | Slint reactive properties + Rust state | Properties for UI, `Rc<RefCell>` for UI-thread, `Arc<Mutex>` for cross-thread |
| Settings Persistence | `confy` (TOML) | Simple, cross-platform config file |

---

## 3. IPC Architecture

The client uses a `Command`/`Event` IPC pattern to communicate with `mello-core`:

```
┌─────────────────────┐        Command (mpsc::Sender)       ┌──────────────────┐
│                     │ ──────────────────────────────────▶  │                  │
│   Slint UI thread   │                                      │  mello-core      │
│   (main.rs)         │  ◀──────────────────────────────────  │  (async loop)    │
│                     │        Event (mpsc::Receiver)         │                  │
└─────────────────────┘                                      └──────────────────┘
```

- **Commands** are sent from Slint callbacks (button clicks, input changes) into the core's async run loop. Examples: `CreateCrew`, `JoinVoice`, `SendMessage`, `SearchUsers`.
- **Events** are received on the UI thread via polling and update Slint properties. Examples: `CrewCreated`, `MessageReceived`, `VoiceActivity`, `StreamFrame`, `VoiceSfuDisconnected`.
- Slint `on_*` callbacks are organized in `callbacks/` submodules (auth, crew, voice, chat, settings, streaming, onboarding) and wired at startup. A timer-driven event loop in `poll_loop.rs` drains the event receiver and dispatches to `handlers/` submodules which update Slint properties. Shared state lives in an `AppContext` struct (`app_context.rs`) threaded through all modules.

### State ownership

| State type | Mechanism | Example |
|-----------|-----------|---------|
| UI-only, single-thread | `Rc<RefCell<T>>` | Invited users list, discover cursor, loading flags |
| Cross-thread (UI ↔ tokio) | `Arc<Mutex<T>>` | Avatar base64 data (picked on main thread, sent via Command) |
| Persistent across restarts | `Settings` struct via `confy` | Audio device IDs, onboarding step, pending crew details |
| Slint-managed | `in`/`in-out` properties | Crew list, chat messages, UI toggles |

---

## 4. UI Structure

### Main Layout

```
┌─────────────────────────────────────────────────────────────────────────┐
│                            MAIN WINDOW                                  │
│  ┌─────────────┐ ┌───────────────────────────────┐ ┌─────────────────┐  │
│  │   CREW      │ │        STREAM VIEW /           │ │    CHAT         │  │
│  │   PANEL     │ │     VOICE CHANNEL VIEW         │ │    PANEL        │  │
│  │             │ │                                 │ │                 │  │
│  │  - Crews    │ │   - Video frames               │ │  - Messages     │  │
│  │  - Members  │ │   - Voice channel members      │ │  - System msgs  │  │
│  │  - Status   │ │   - Stream info                │ │                 │  │
│  └─────────────┘ └─────────────────────────────────┘ └─────────────────┘  │
│  ┌──────────────────────────────────────────────────────────────────┐   │
│  │                        CONTROL BAR                               │   │
│  │  [Avatar] Name    [Mic] [Headphones] [Settings]   [Message...]   │   │
│  └──────────────────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────────────────┘
```

### Panels and Modals

All UI components live in `client/ui/panels/`:

| File | Description |
|------|-------------|
| `crew_panel.slint` | Left sidebar: crew list (active/idle sections), member avatars, online counts, voice member rows |
| `member_volume_popover.slint` | Popover: the volume and the local mute of one voice member (section 4.1) |
| `chat_panel.slint` | Right sidebar: message list, chat input |
| `control_bar.slint` | Bottom bar: user info, go-live (broadcast), mic/deafen toggles, clip, settings, game states |
| `stream_view.slint` | Center: video frame rendering, stream host info, viewer controls |
| `voice_channel_view.slint` | Center (when not streaming): voice channel list with member avatars |
| `settings_modal.slint` | Overlay: audio devices, general preferences, profile editing |
| `new_crew_modal.slint` | Overlay: crew creation (name, description, avatar upload, invite members, visibility) |
| `onboarding.slint` | Full-screen: 3-step new user flow (discover, profile, identity) |
| `discover_panel.slint` | Crew discovery with bento grid, infinite scroll, join/invite-code entry |
| `sign_in.slint` | Social login buttons, email form |
| `debug_panel.slint` | Developer diagnostics (toggled via `Command::SetDebugMode`) |
| `update_banner.slint` | Auto-update notification bar |

### Root wiring

`client/ui/main.slint` is the root component. It declares all top-level properties, conditional panel visibility (`if logged-in`, `if onboarding-step < 4`, `if show-discover`), and wires callbacks from child components to root-level callbacks that `main.rs` binds to.

### 4.1 Member Volume

The local user sets the playback volume of each other voice member, and can mute a member for themselves. Only this user hears the change. The mockup and the design decisions are in `mello-backlog/mockups/voice-member-volume/`.

**Open and close.**

| Action | Result |
|---|---|
| Pointer on the row of another member | The row gets the raised ground and a speaker glyph at the right end. The slot of the glyph is always on the row, so the name does not move. |
| Click the speaker glyph, or Tab to it and push Enter or Space (`accessible-label` "Volume for <name>") | The popover opens. |
| Right-click the row | The popover opens. |
| Your own row | No glyph and no right-click action. |
| Escape, a click outside the popover, or the member leaves the channel | The popover closes. |

The popover (`MemberVolumePopover`) opens to the right of the 240 px sidebar, over the feed, with its top 12 px above the row. Near the bottom of the window it moves up. While it is open, the row keeps the raised ground and a `graphic-med` keyline.

**The popover.** Avatar and name, "Only you hear this change.", USER VOLUME with a value chip, a slider, a line above 100 %, a Mute switch, RESET TO 100%.

| Control | Behaviour |
|---|---|
| Slider | 0 to 200 %, default 100 %. A pointer value within 4 % of 100 % snaps to 100 %. Keys: arrows 5 %, Shift+arrows 1 %, PgUp/PgDn 25 %, Home 0 %, End 200 %. A key step across 100 % stops on 100 %. The scroll wheel steps 5 %. |
| Above 100 % | "Above 100% a loud voice can distort." libmello applies the gain before the limiter. |
| Mute | Gain 0.0 for that member. The volume does not change. The chip shows MUTED and the slider is dim and takes no input. |
| RESET TO 100% | Sets 100 %. Dim at 100 %. It does not change the mute. |

The popover is controlled: it sends `member-volume-changed(user-id, percent)` and `member-local-mute-changed(user-id, muted)`. Rust stores the value and gives it back in `volume-member`.

**Row glyphs.** Grey is the member's own equipment. Red is your speaker: a state you set and can undo in the popover. All three glyphs can show together.

| State | Glyph | Colour | Tooltip |
|---|---|---|---|
| They muted their mic | `mic-off.svg` | `graphic-med` | "Muted" |
| They deafened themselves (a deafen also mutes, so the mic glyph shows too) | `headphone-off.svg`, next to the mic glyph | `graphic-med` | "Deafened" |
| You muted them | `volume-x.svg` | `Theme.live` | "Muted for you" |

Only your local mute dims the row (avatar and name). The green name of a member who speaks stays. A volume that is not 100 % shows a `NN%` badge; the local-mute glyph replaces it.

**State.** `client/src/member_audio.rs`, `MemberAudioTable` in `AppContext.member_audio`:

| Rule | Value |
|---|---|
| Key | The user id. The value follows the person into every channel and crew. |
| Value | `{ volume_percent: u16 (default 100, max 200), muted_locally: bool }`. A member at the default has no entry. |
| Lifetime | In memory for the life of the app. Leaving a channel keeps it. A restart clears it. Nothing goes to the settings file or to the server. |
| Apply | `Command::SetPeerVolume { peer_id: user_id, volume }`, `volume = 0.0` when muted, else `percent / 100`. |
| Apply again | `handlers::voice::sync_member_audio` runs after each event that changes the voice channel models. It sends the stored value for each member who appears in the joined channel: a join, a rejoin, a reconnect. |

libmello names the receive state of a member after the user id (the SFU slot maps a mid to the user id). libmello keeps the gain of a peer for the life of its context, also across `remove_peer` and a slot release. The client sends the value again so that it does not depend on that.

Stream audio has its own volume. This slider does not change it.

The e2e state port reports `id`, `volume` and `local_muted` for each voice member.

### Settings tabs

The settings modal (`settings_modal.slint`) has five tabs, in this order:
PROFILE, GENERAL, AUDIO, GAMES, ABOUT.

#### ABOUT tab

The ABOUT tab is read-only. It has these parts, top to bottom.

| Part | Content |
|---|---|
| M3LLO | The wordmark, then four rows: VERSION, BUILD (the short commit), BUILT (the build date), COPYRIGHT ("© 2026 Mollo Tech AB, Göteborg") and CREATED BY ("b0bben (bob@mollo.io)", a link that opens `mailto:bob@mollo.io`). The stamp comes from `client/src/build_info.rs`. |
| THANK YOU | One sentence: m3llo runs on the work of many people and services, and this page names them. |
| THE OG CREW THAT HELPED SHAPE M3LLO | A row of seven octagons, each with the tester's Steam avatar and the name under it in Oxanium 600: Ostkatt, Propotato, xtrÖm, Erle, Halvtokig, LipzzarN, Keyden. Each octagon is a control labelled "<name> on Steam" that opens the Steam profile in the browser. |
| SERVICES WE USE | Names only, each a link to its site where one exists (a control labelled "<name> website" that opens the address in the browser through `open_url::open`), and only services that are free or open source: Nakama (Heroic Labs), IGDB, DiceBear, Giphy, Google STUN, Steam, Epic Games, GOG, Riot Games. Paid vendors and sign-in providers are not listed. |
| OPEN SOURCE WE SHIP | Names only, each a link like the services: Slint, Velopack, RNNoise, WebRTC audio processing, ONNX Runtime, Silero VAD, OpenH264, Opus, OpenSSL, libdatachannel, Pion, OpenMoji, Oxanium, Barlow. Then "And the Rust crates listed in the third-party notices." Then the button "Third-party notices". |
| CREDITS | "Game data and artwork by IGDB", "Made with Slint", and "Emoji by OpenMoji (https://openmoji.org), CC BY-SA 4.0". IGDB, Slint and OpenMoji are links to igdb.com, slint.dev and openmoji.org. |

The "Third-party notices" button opens a view in the same modal. A back control returns to ABOUT.
The view lists each licence text once, with the names of the dependencies that share it.

**Tester avatars.** Nothing is fetched until the ABOUT tab is opened. Opening it sends `Command::FetchTesterAvatars` for the avatars that are missing or older than a week.
Core (`mello-core/src/testers.rs`) reads the public profile XML (`<profile url>?xml=1`), takes the `avatarFull` URL, downloads the image and saves it as `tester_avatars/<slug>.jpg` in the app data dir.
A file younger than a week is used as it is, so Steam is asked once a week per tester. `Event::TesterAvatarLoaded` rebuilds the tiles.
A private profile, a profile with no avatar or a failed fetch leaves the monogram in the tile's identity hue. A tile opens its profile through `open_url::open`, the path chat links use.

**Attribution obligations.** Two licences require a visible line in the app.

- The Slint royalty-free licence requires visible attribution. The CREDITS line "Made with Slint" meets it.
- OpenMoji uses CC BY-SA 4.0. The CREDITS line for OpenMoji, with its address and the licence name, meets it.

Do not remove either line. Do not move them out of the ABOUT tab.

**Notices asset.** `client/build_notices.rs` runs from `client/build.rs` at build time.
It writes `third_party_notices.txt` into `OUT_DIR`. The client embeds it, and `client/src/about.rs` parses it.

- Records are separated by U+001E. Each record has three fields separated by U+001F: the names (one `name version` pair per line), the licence name and the licence text.
- Dependencies with the same licence text share one record.
- Sources: the Rust crates (`cargo tree` for the shipped targets, normal dependencies only, with the licence files in the crate source), `libmello/vcpkg.json` and `libmello/third_party`, the font licences in `client/ui/fonts`, and OpenMoji and Slint by name.

---

---

## 5. Theme System

`client/ui/theme.slint` defines a `Theme` global with design tokens:

- **Colors:** `bg-app`, `surface`, `surface-hover`, `text-primary`, `text-secondary`, `text-tertiary`, `accent`, `graphic-light`, `graphic-med`, etc. Supports dark/light via `Theme.dark` boolean.
- **Fonts:** Two families — `font-mono` (monospace, used for labels and code-style text) and `font-sans` (sans-serif, used for body text).
- **Radii:** `r-outer` (panel corners), `r-inner` (input fields, buttons).

SVG icons from mockups are extracted into `client/ui/icons/` as `.svg` files and referenced via `Image { source: @image-url("../icons/foo.svg"); colorize: Theme.accent; }`.

---

## 6. Onboarding Flow

Onboarding is a 3-step full-screen flow for new users (when `onboarding_step` is 0 to 3, or 5 for the invite welcome screen of §6.2):

| Step | Screen | What happens |
|------|--------|-------------|
| 1 | Discover Crews | Bento grid of public crews (fetched unauthenticated via `http_key`). "Create Your Own Crew" opens the new-crew modal in onboarding mode (invite section disabled, button says "Save & Continue"). Crew details stored locally, creation deferred. Row 1 also has the invite-code card (§6.4). The grid shows at most four crews. |
| 2 | Profile Setup | User sets nickname and picks an avatar. |
| 3 | Identity Linking | Required. The user links one identity: a provider (Steam, Twitch, Google, Apple, Discord) or email + password. There is no skip. A successful link enters the main app. |

"Continue" on step 2 sends `FinalizeOnboarding`, which device-auths, creates the account, and creates or joins the crew (with the stored details and avatar). Step 3 follows.

The `pending_crew_name`, `pending_crew_description`, `pending_crew_open`, `pending_invite_code`, `pending_invite_crew_name`, `pending_invite` and `onboarding_via_invite` fields are persisted in `Settings` (survives restart). The crew avatar base64 is held in memory only (`Arc<Mutex<Option<String>>>`).

A restart on step 3 sends `DeviceAuth` with the stored device id. Linking needs a session in core, and only a finished onboarding restores one.

### 6.1 Sign-in Entry Points

Step 1 always shows a way into an existing account. The sign-in entry is the right column of the step 1 header.

| Case | Step 1 shows |
|------|--------------|
| Device auth found the device account (`DeviceAuthed { created: false }`), for example after a logout | The device card: the avatar, "ON THIS DEVICE", the name and a red "Sign in". "Sign in" opens the app as the device account. Under the card, "Use another account" opens the sign-in panel. |
| Any other case | "I already have an account". It opens the sign-in panel. |

"Any other case" includes:

- A fresh install, with no `Settings::device_id`.
- A device id whose account is not on the server. Device auth then creates a new account (`created: true`).
- Device auth that is still running, or that failed.

Logout clears the device card. The card shows again when device auth answers. A card that arrives late does not move the link: the link keeps its place.

The two controls have different labels. Step 1 never shows two controls labelled "Sign in" (#70).

The sign-in panel:

- Stays open while a provider flow runs. A failure shows on the panel.
- Shows plain messages, never the server text. "User account not found" becomes "No account found." with a "Start as a new player" button. "Invalid credentials" becomes "Wrong email or password.".
- "Back" and "Start as a new player" close the panel and clear the error. Both return to step 1.

### 6.2 Invite Link on a Fresh Install

A fresh install opened from `mello://join/{code}` skips step 1 (#68). "Fresh install" means no session, no device account, and a state before the account exists (`Loading`, step 1, the welcome screen or step 2).

1. At startup, before crew discovery, the client sends `ResolveCrewInvite`. Without a session, core calls `resolve_crew_invite` with the `http_key`.
2. `CrewInviteResolved`: the client stores the code and the resolved invite, and opens the welcome screen (`InviteWelcome`).
3. "Join {crew}" opens step 2. Step 2 shows "JOINING CREW" and the crew name.
4. "Continue" sends `FinalizeOnboarding` with `invite_code`. Core joins the crew with `join_by_invite_code`, so a private crew works too. Step 3 follows as usual.

**The welcome screen** (`client/ui/panels/invite_welcome.slint`) shows no step indicator. It shows:

| Element | Content |
|---|---|
| Label | "YOU'RE INVITED" |
| Inviter row | The inviter as an octagon, and "{inviter} invited you to join". With no inviter: "You're invited to join". |
| Crew | The crew tile with initials, the crew name, the highlight (if present), up to 5 member octagons and "{n} members" |
| Next steps | "Pick a name and an avatar, check your mic, and you're in. It takes about a minute." |
| Primary button (red) | "Join {crew}". Long names elide. Opens step 2. |
| Text link | "Not now — show me other crews". Opens step 1 and forgets the invite. It also clears the invite-code card (§6.4). Finalize then does not join the invited crew. |

The crew tile shows the crew avatar. The resolve answer has no avatar, so the client sends `FetchCrewAvatars` with the crew ID. Core calls `get_crew_avatar` with the `http_key`, so no session is needed. The tile shows the initials until `CrewAvatarLoaded` arrives, and when the crew has no avatar. Step 2 ("JOINING CREW") and the join modal show the same avatar.

The welcome screen is onboarding step 5 in `Settings` and in the Slint property. Steps 0 to 4 keep their numbers from earlier builds. In the flow it comes before step 2.

A restart on the welcome screen shows it again. The screen reads the invite from `Settings::pending_invite`, so it needs no network call. With no stored invite, startup opens step 1.

**Back on step 2.** On the invite path, step 2 shows a "Back" control above the step indicator. It opens the welcome screen. The invite stays stored, so "Join {crew}" opens step 2 again. "Back" does nothing while finalize runs. Without an invite, step 2 has no "Back": the step indicator goes back.

**Step numbers.** The invite path has two steps: step 2 shows "STEP 01 / 02", step 3 shows "STEP 02 / 02". `Settings::onboarding_via_invite` keeps this through a restart. It is set when the invite resolves, and cleared on step 1 and when onboarding is done. Without an invite, onboarding has three steps.

| Case | Result |
|------|--------|
| The code does not resolve | Step 1, with the message above the crews. No join modal. |
| "Not now" on the welcome screen | Step 1, three steps. The invite is forgotten. |
| Finalize: the join fails with a server or network error (`OnboardingInviteFailed`, `Failed`) | Step 2, with the message above "Continue". "Continue" retries. |
| Finalize: the code is no longer valid, the crew is full, or the server refuses the user | Step 1, with the message. The invite is forgotten. |
| The user goes back to step 1 and picks or creates a crew | The crew replaces the invite. |
| The user goes back to step 1 and signs in to an existing account | The stored invite opens the join modal after sign-in. |
| A device account exists, or the user is logged in | No change for a link: the join modal opens after sign-in. An invite typed in the card (§6.4) opens the welcome screen. |

### 6.3 Lost Session

At startup with onboarding done (`onboarding_step` 4, or a step the build does not know) the client sends `TryRestore`. When the restore fails and a device account exists:

1. The client sends `DeviceAuth`. The window stays on the restore wait.
2. `DeviceAuthed { created: false }`: the account exists. The app opens, the same as a restored session.
3. `DeviceAuthed { created: true }` or a failed device auth: step 1.

With no device account, a failed restore goes to step 1.

### 6.4 Invite-Code Card on Step 1

The web lounge copies the invite link to the clipboard on a download, and the first launch reads it ([CREW-INVITES.md](features/CREW-INVITES.md) §7.1). The clipboard can hold something else by then. A user who installed the app then has no link to follow. Step 1 has the invite-code card for this user. See [CREW-INVITES.md](features/CREW-INVITES.md) §8.5.

| Item | Rule |
|---|---|
| Place | Row 1 of the bento grid, right of "Create your own crew". With no discoverable crews, next to "Create your own crew". |
| Shown to | Every user on step 1, also after a logout. |
| Content | Label "GOT AN INVITE?", the line "Paste the link or the code your friend sent you.", a field, an error line, and a white button "Open invite". |
| Input | A web link, a `mello://join/` link, or a bare code. One parse function (`deep_link::parse_invite_input`) reads all three. |
| Invalid input | "This invite code is not valid." in the card. The field gets a red outline. No network call. |
| Submit | The button, or Enter in the field. The button reads "Checking…" and ignores clicks until the answer arrives. |
| Success | The same path as a deep link: the client stores the invite and opens the welcome screen (§6.2). |
| Failure | The message shows in the card. An invalid or expired code reads "This invite code is not valid." Other failures read "Could not load this invite. Try again." |
| Edit | Editing the field clears the error. |
| "Not now" | Opens step 1 with the field empty. |

**After a logout.** The machine has a device account, and the user has no session. The card takes the same path: the welcome screen opens, not the join modal. Finalize authenticates the device id, which opens the existing account, and joins the crew by its invite code.

---

## 7. Window Behavior

| Behavior | Implementation |
|----------|----------------|
| Minimum size | 1024 x 768 |
| Default size | 1280 x 800 |
| Resizable | Yes |
| System tray | Yes (minimize to tray) |
| Close button | Minimize to tray (configurable) |
| Start on boot | On by default. Onboarding sets it to the share toggle. Optional setting afterwards |

---

## 8. Keyboard Shortcuts

| Shortcut | Action |
|----------|--------|
| `Ctrl + M` | Toggle mute |
| `Ctrl + D` | Toggle deafen |
| `Ctrl + ,` | Open settings |
| `Escape` | Close modal / Deselect |
| `Enter` | Send message (when input focused) |

---

## 9. Performance Targets

| Metric | Target |
|--------|--------|
| Startup time | <3 seconds |
| Frame render | <16ms (60fps) |
| Input latency | <5ms |
| Memory (idle) | <50MB |
| Memory (streaming) | <100MB |
| Binary size | <10MB (client only) |

---

*This spec defines the desktop client. For core logic, see [02-MELLO-CORE.md](./02-MELLO-CORE.md).*
