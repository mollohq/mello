#!/usr/bin/env python3
"""Give every COLRv0 base glyph of a Twemoji font an outline.

Mozilla's Twemoji build leaves the base glyph of each emoji empty: the colour
lives in the COLR layers only. CoreText then reports a zero bounding box for
the base glyph, and Skia drops the glyph before CoreText gets to draw the
layers, so Slint on macOS renders nothing (OpenMoji's base glyphs carry a
fallback outline, which is why that font drew). This script makes each base
glyph a TrueType composite of its own layer glyphs: no new outline data, a
real bounding box, and a monochrome fallback for renderers without COLR.

Usage:
    python3 tools/fonts/twemoji_base_outlines.py <in.ttf> <out.ttf>

Needs fonttools (`pip install fonttools`). Run once per font update; the
result is what ships in client/ui/fonts.
"""

import sys

from fontTools.ttLib import TTFont
from fontTools.ttLib.tables._g_l_y_f import Glyph, GlyphComponent


def main(src: str, dst: str) -> None:
    font = TTFont(src)
    colr = font["COLR"]
    glyf = font["glyf"]
    hmtx = font["hmtx"]
    if colr.version != 0:
        raise SystemExit(f"expected COLR v0, got v{colr.version}")

    done = 0
    for base, layers in colr.ColorLayers.items():
        g = glyf[base]
        if g.numberOfContours != 0 or not layers:
            continue
        comp = Glyph()
        comp.numberOfContours = -1
        comp.components = []
        for layer in layers:
            name = layer.name
            if name == base or glyf[name].numberOfContours == 0:
                continue
            c = GlyphComponent()
            c.glyphName = name
            c.x = 0
            c.y = 0
            c.flags = 0
            comp.components.append(c)
        if not comp.components:
            continue
        glyf[base] = comp
        comp.recalcBounds(glyf)
        done += 1
        advance, _ = hmtx[base]
        hmtx[base] = (advance, comp.xMin)

    font["head"].flags |= 0x1  # baseline at y=0, as before
    font.save(dst)
    print(f"{done} base glyphs got a composite outline -> {dst}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    main(sys.argv[1], sys.argv[2])
