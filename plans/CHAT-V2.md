# Crew chat v2

> Status: approved 2 Nov 2026. Phase 0 built on `feat/chat-v2` (worktree `mello-wt-chat-v2`).
> Before the next release: add the `GIPHY_API_KEY` secret to the mollohq/mello repository.
> The release workflow passes it, and a production build refuses to compile without it.
> Mockup: `designs/chat-v2.html`. Spec to update after: `specs/features/TEXT-CHAT.md`.

## 1. What is broken today, and why

| Report | Root cause | Where |
|---|---|---|
| GIF popover shows no GIFs | Release builds get no `GIPHY_API_KEY`. The binary falls back to the literal `GIPHY_DEV_KEY`. Giphy answers 401. The error goes to the log only, the grid stays blank. | `release.yml:65-71`, `mello-core/src/giphy.rs:10`, `client/chat.rs:71` |
| GIF popover shows no GIFs, second cause | The frame timer stops itself 50ms after a load, before any fetch returns. Decoded frames land in an inbox nothing drains. Since 9fc96e71 (July). | `client/src/gif_animator.rs:143-152`, `handlers/chat.rs:168-206` |
| GIFs in the feed stay on "Loading GIF…" or stop | Same timer bug. Every refresh rebuilds the row model with no image, and does not re-fetch a URL already seen. A GIF that ran its two loops goes back to "Loading" until hover. | `converters.rs:212-213, 298-306` |
| GIFs look soft | Bubbles use the `fixed_width_small` rendition (about 100px) scaled up to 400px. | `handlers/chat.rs:206`, `chat_panel.slint:998-1012` |
| Feed lands on the same spot every time | The feed is a virtualized `ListView`. Its content height is an estimate: average visible row height × row count. "Scroll to bottom" targets that estimate and snaps to a row index, so it lands on the same row, not the bottom. Prepend after history uses the same estimate. | `chat_panel.slint:611, 1119, 1132`; Slint `repeater.rs:424-438` |
| Feed never scrolls past page one | History load trims to the newest 50 after the cursor advanced. Older pages are lost. | `nakama/client.rs:2034-2036` |
| Emoji in the picker are small | 18px glyph in a 32px cell. | `chat_panel.slint:1402-1418` |
| An emoji-only message is small | No emoji-only detection. Body is always 13px. | `converters.rs:174-183` |
| Picker closes on each pick | Spec says stay open. | `chat_panel.slint:1426-1428` |
| No image upload | Nothing exists. Server accepts `text` and `gif` only. | `chat_validation.go:78` |

Nothing above is a tuning problem. Each one is a missing piece or a wrong primitive.

## 2. Target

Crew chat for five people who play together. The bar is Discord in a 400px panel:
the feed opens at the bottom, images and GIFs are first class, emoji are visible,
reactions exist, and nothing fails silently.

Not in scope: DMs, threads, voice messages, stickers, read receipts, custom emoji.

## 3. Phases

Each phase is one branch and one PR. Order is fixed. Phase 0 ships alone first.

### Phase 0: make what exists work

Built. Each row below is a commit on `feat/chat-v2`. Not yet verified in the running app: the
Twemoji glyphs and the jumbo size, because the headless viewer draws no colour fonts.

| # | Change | Notes |
|---|---|---|
| 0.1 | Giphy key: add the secret to both release build steps. Fail the `production` build at compile time when the key is absent. | A `compile_error!` behind the production feature. A dev build keeps the fallback. |
| 0.2 | Giphy errors reach the UI. The popover shows "GIFs are not available right now" and a Try again button. Rate limit (429) gets its own line. | New `GifsFailed` event. |
| 0.3 | GIF frames: the timer runs while a fetch is in flight or a frame is animating. Frames are cached by URL across refreshes. A row references its image by URL, the animator owns the pixels. | Removes the duplicate fetch and the "Loading" regression. |
| 0.4 | Renditions: picker tiles use `fixed_width_small`, bubbles use `fixed_width` (200px) at up to 300px. | Memory: 200px × 32 frames × 4B ≈ 2.5 MB per GIF at worst. Keep the 8-row prefetch. |
| 0.5 | Feed: replace `ListView` with `Flickable` + `VerticalLayout` over a bounded window of at most 200 messages. Real heights. Scroll to bottom = `content-height - height`, exact. Prepend keeps the first visible row at the same y, exact. | Rows are cheap. Images are the only heavy thing, and they decode lazily already. Measure RAM before and after on the perf harness. |
| 0.6 | Model updates in place: push, insert, set_row_data. No `VecModel` rebuild per event. | Keeps Slint's layout cache valid and makes 0.5 cheap. |
| 0.7 | Pagination: fix the trim so page two and later load. Dedupe by message id on prepend. | |
| 0.8 | Emoji: picker 24px glyph in a 36px cell, nine across. Picker stays open on a pick. Emoji-only body (at most three emoji, no text) renders at `Theme.font-xxl` (32px), the Discord rule. | `is-emoji-only` on `ChatMessageData`, detection in converters. |
| 0.9 | Emoji font: swap OpenMoji for Twemoji (Mozilla build, `Twemoji.Mozilla.ttf`, 1.4 MB, COLRv0). Same `SLINT_FONT_PATH` path, same memory profile. Attribution in credits: "Twemoji graphics, Twitter and contributors, CC BY 4.0". Remove the OpenMoji credit. | Repertoire is Unicode 14. Newer emoji render as tofu until we build our own font from the maintained fork's SVGs with the twemoji-colr pipeline (later, not blocking). Candidates compared in `designs/emoji-candidates.html`: Fluent flat is prettier but has no country flags and no font. |
| 0.10 | Tests: a flow test for scroll-to-bottom after `MessagesLoaded` and after a prepend, a test for the animator timer with an in-flight fetch, a test for the emoji-only flag. | Each must fail without its fix. |

Done when: a release build shows trending GIFs, a feed with 120 messages opens at the last message, scroll up loads page two and the view does not jump, 🔥🔥 is big and drawn by Twemoji on macOS and Windows.

### Phase 1: images

| # | Change |
|---|---|
| 1.1 | Envelope `type: "image"` with `images: [{url, width, height, bytes, mime}]`, max 4. Server validates type, count and that each URL is on our public bucket host. |
| 1.2 | RPC `chat_upload_url`: checks crew membership, returns a presigned PUT for `{crew}/{ulid}.{ext}`, content type `image/jpeg`, `image/png`, `image/webp` or `image/gif`, 15 min. Own bucket `mello-chat` (R2 in prod, MinIO local), new env `CHAT_S3_BUCKET` and `CHAT_S3_PUBLIC_URL` next to the snapshots pair in `s3.go`. Not the clips bucket. |
| 1.3 | Client: resize to 1600px on the long side, re-encode (JPEG for photos, PNG kept for screenshots with few colours, GIF passed through), cap 8 MB after encode. PUT with reqwest, then send the message. |
| 1.4 | Composer: an Attach pill on the left of the pills row opens `rfd` (already a dependency). Paste from clipboard with `arboard` (already a dependency, has image get). Drop onto the panel through Slint `DropArea` if the winit backend delivers file drops; if not, note it and ship paste + dialog. |
| 1.5 | Pending chips between the pills row and the field, with size, progress and remove. |
| 1.6 | Feed: image renders like a GIF, max 300px wide, one cut corner, size tag. Click opens an overlay over the panel with Save, Copy, Close. |
| 1.7 | Delete message removes its objects from the bucket (server side, in the existing delete path). |

Privacy: keys are ULIDs, unguessable. The bucket stays in the EU. No EXIF leaves the client: the re-encode strips it. A separate bucket keeps chat media apart from clips: its own lifecycle, its own public URL, and a delete of a crew can clear it in one prefix.

### Phase 2: feel

| # | Change |
|---|---|
| 2.1 | Date separators: Today, Yesterday, then the date. |
| 2.2 | Reactions, server side. A storage object `chat_reactions/{crew}/{message}` holds `{emoji: [user_id]}`. RPC `chat_react {message, emoji, op}` validates membership and emoji, updates the object, and broadcasts a stream event to the crew. Client folds the event into the row. History load fetches the objects for the loaded window in one `storage_read` batch. Hover row gets React. Own reaction gets the white outline. |
| 2.3 | Read cursor per crew in Nakama storage (`chat_read/{crew}` = last read message id + time). The unread rule and the sidebar badge survive a restart. The feed opens at the last read message when there are unread ones, else at the bottom. |
| 2.4 | Emoji picker: search, Recent row, category tabs, full Unicode set from a generated table (the OpenMoji font is already bundled, so every glyph renders). Binary cost of the table: about 150 KB. |
| 2.5 | Giphy behind a Worker (`gif.m3llo.app`): the key leaves the public binary, trending is cached at the edge, Giphy never sees a user IP, self-hosters set their own key. Beta keys allow 100 calls per hour per key, which a shared key in a public binary cannot survive. |

### Phase 3: later, not planned in detail

Link previews through a Worker (clients must not unfurl, that leaks reader IPs).
Typing indicator. Pin a message. Share a clip to chat. Message search.

## 4. Decisions, settled

- The action row stays between the list and the field as three icon buttons, Attach, GIF, Emoji. The small red send sits inside the field, arrow up.
- Images get their own bucket, `mello-chat`.
- The emoji font is Twemoji.
- The feed drops the virtualizer for a Flickable over a 200-message window. RAM measured before and after on the perf harness.
- Image caps: 4 per message, 1600px on the long side, 8 MB each after encode.
- Reactions are a server-side model (2.2), not folded messages.
- Jumbo emoji at three or fewer, like Discord.
- Giphy stays client side. See §4.1.

### 4.1 GIF provider

Giphy's API rules, read 2 Nov 2026: "Do not proxy requests to GIPHY, either API calls or media URL loads." "GIPHY requires the Search API call be made from the client side." "Do not cache API responses or media URLs." So the Worker idea is out. The key lives in the client, as Giphy intends, and the release secret fix (0.1) is the whole fix. Beta keys are limited to 100 calls per hour per key; a production key needs an application and "a member of our team will reach out via email to discuss pricing". Bob applies for production status before launch. Attribution stays: "Powered by GIPHY" in the popover.

Fallback if Giphy production pricing does not fit: KLIPY. Test key 100 calls per hour, production key "limitless" through a form in their Partner Panel, ads optional and off by default per app, attribution required. Same client-side pattern, so `giphy.rs` becomes a `GifProvider` trait with one implementation per service. Decide after the Giphy email.

