import sys, pathlib
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.boundsPen import BoundsPen

CHARS = "費禕"
src, out = map(pathlib.Path, sys.argv[1:])
for path in sorted(src.glob("*.ttf")):
    font = TTFont(path)
    cmap, gs = font.getBestCmap(), font.getGlyphSet()
    if not all(ord(c) in cmap for c in CHARS):
        print(f"{path.stem}: missing glyphs"); continue
    upm = font["head"].unitsPerEm
    for layout, cells in {"v": [(256, 156), (256, 356)], "h": [(156, 256), (356, 256)]}.items():
        s = 196 / upm
        ds = []
        for c, (cx, cy) in zip(CHARS, cells):
            g = gs[cmap[ord(c)]]
            b = BoundsPen(gs); g.draw(b)
            x0, y0, x1, y1 = b.bounds
            pen = SVGPathPen(gs, ntos=lambda v: str(round(v)))
            # font units are y-up; centre the glyph's ink box in its cell
            g.draw(TransformPen(pen, (s, 0, 0, -s, cx - s * (x0 + x1) / 2, cy + s * (y0 + y1) / 2)))
            ds.append(pen.getCommands())
        d = " ".join(ds)
        (out / f"{path.stem}-{layout}.d").write_text(d)
        (out / f"{path.stem}-{layout}.svg").write_text(
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512"><rect width="512" height="512" fill="#181B1A"/>'
            f'<path fill="#E5553B" d="{d}"/></svg>')
        print(path.stem, layout, len(d), "chars")
