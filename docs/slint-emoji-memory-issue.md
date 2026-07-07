# Draft upstream issue for slint-ui/slint

> Search for existing issues first (keywords: "Apple Color Emoji", "emoji memory",
> "font_cache new_from_data"). If none, file as-is. Delete this file after filing.

---

**Title:** Skia renderer: rendering any color emoji on macOS permanently allocates 188 MB (full copy of Apple Color Emoji.ttc)

## Summary

With the Skia renderer on macOS, rendering a single color emoji (e.g. `😀` in a `Text`
element) permanently increases the process physical footprint by ~188 MB, with transient
peaks of 2–3× that during the first render.

## Reproduction

Any Slint app with the Skia renderer on macOS that renders `Text { text: "😀"; }`.
Measure with `footprint <pid>` (or Activity Monitor) before/after the emoji becomes
visible. Observed on Slint 1.17.0, macOS 15.7.4, Apple Silicon.

- Before emoji: ~205 MB phys_footprint
- After emoji: ~400 MB phys_footprint (peak ~680–970 MB)

`malloc_history -allBySize` shows a single live allocation of **188,589,692 bytes**
allocated under `draw_glyph_run → skia FontMgr::new_from_data → SkDynamicMemoryWStream
→ _realloc`. `/System/Library/Fonts/Apple Color Emoji.ttc` is **188,589,668 bytes** —
the allocation is a byte-exact heap copy of the entire font file (+24 bytes header).

## Cause (from reading 1.17.0 sources)

`i-slint-renderer-skia/font_cache.rs`:

1. `FontCache::load_typeface_internal` calls `self.font_mgr.new_from_data(font.data.as_ref(), …)`,
   which copies the whole font blob into Skia-owned heap memory. For ordinary fonts this
   is a few MB and goes unnoticed; for Apple Color Emoji.ttc it is 188 MB.
2. The typeface (and via the `HashedBlob` cache key, the blob) is retained in the
   thread-local `FONT_CACHE` LRU for the lifetime of the app.
3. The macOS TTC workaround in the same function (for https://issues.skia.org/issues/310510989)
   re-extracts the face with `write_fonts::FontBuilder` — producing an additional full-size
   copy — and then calls `new_from_data` on that as well, which explains the 2–3×
   transient peaks.

fontique's blob itself appears to be fine (mmap-capable via `memmap2`); the copies happen
at the fontique→Skia bridge.

## Suggested directions

- Load system fonts by file path (`SkData::MakeFromFileName` mmaps) instead of copying
  bytes with `new_from_data`, or
- On macOS, resolve system fonts through Skia's CoreText font manager
  (`FontMgr` default on mac) by name/descriptor rather than by blob, so the system
  emoji font is never duplicated in process memory.

## Workaround for other users

Ship a small COLRv0 emoji font (e.g. OpenMoji/Twemoji, ~10 MB) and add it to
`SLINT_FONT_PATH` before Slint initializes — fonts registered there enter the
generic-family fallback chain and win emoji fallback, so the system emoji font is never
loaded. Footprint with emoji: ~400 MB → ~216 MB in our app.
