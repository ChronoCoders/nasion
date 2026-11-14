"""Build the Nasion logo files.

Usage: python3 -I build_logo.py <poppins-600.woff> <out_dir> [variant]

The wordmark is converted to outlines so the SVG files do not depend on any
installed font. The mark is the midline arc of the 10-20 EEG system: it runs
from the nasion (large dot) to the inion, with the five midline electrode
sites placed at 10, 30, 50, 70 and 90 percent of the arc.
"""
import math
import sys
from fontTools.ttLib import TTFont
from fontTools.pens.svgPathPen import SVGPathPen
from fontTools.pens.transformPen import TransformPen

NAVY = "#10263d"
BLUE = "#1b5fae"
WHITE = "#ffffff"
LIGHT_BLUE = "#7fb2ee"

SITES = (0.10, 0.30, 0.50, 0.70, 0.90)


def wordmark_path(font_path, text, cap_height_px, tracking=0.0):
    """Return (path_d, width_px, height_px) for text set in the given font."""
    font = TTFont(font_path)
    glyphs = font.getGlyphSet()
    cmap = font.getBestCmap()
    upm = font["head"].unitsPerEm
    cap = font["OS/2"].sCapHeight
    scale = cap_height_px / cap
    pen = SVGPathPen(glyphs)
    x = 0.0
    for ch in text:
        name = cmap[ord(ch)]
        # Flip the y axis: font units grow upward, SVG grows downward.
        tpen = TransformPen(pen, (scale, 0, 0, -scale, x, cap_height_px))
        glyphs[name].draw(tpen)
        x += glyphs[name].width * scale + tracking
    x -= tracking
    return pen.getCommands(), x, cap_height_px


def mark(cx, cy, r, arc_color, site_fill, site_stroke, dot_color, variant="nodes", stroke=4.0):
    """Return SVG elements for the arc mark. Nasion sits at the right end."""
    parts = []
    left = (cx - r, cy)
    right = (cx + r, cy)
    parts.append(
        f'<path d="M{left[0]:.2f} {left[1]:.2f} A{r} {r} 0 0 1 {right[0]:.2f} {right[1]:.2f}" '
        f'fill="none" stroke="{arc_color}" stroke-width="{stroke}" stroke-linecap="round"/>'
    )
    for frac in SITES:
        theta = math.pi * frac  # measured from the nasion end
        px = cx + r * math.cos(theta)
        py = cy - r * math.sin(theta)
        if variant == "ticks":
            ox = cx + (r + 8) * math.cos(theta)
            oy = cy - (r + 8) * math.sin(theta)
            parts.append(
                f'<path d="M{px:.2f} {py:.2f} L{ox:.2f} {oy:.2f}" stroke="{site_stroke}" '
                f'stroke-width="{stroke * 0.8:.2f}" stroke-linecap="round"/>'
            )
        else:
            parts.append(
                f'<circle cx="{px:.2f}" cy="{py:.2f}" r="{stroke * 0.95:.2f}" fill="{site_fill}" '
                f'stroke="{site_stroke}" stroke-width="{stroke * 0.55:.2f}"/>'
            )
    parts.append(f'<circle cx="{right[0]:.2f}" cy="{right[1]:.2f}" r="{stroke * 1.75:.2f}" fill="{dot_color}"/>')
    return "\n  ".join(parts)


def svg(width, height, body, title):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {width:.0f} {height:.0f}" '
        f'width="{width:.0f}" height="{height:.0f}" role="img" aria-label="{title}">\n'
        f'  <title>{title}</title>\n  {body}\n</svg>\n'
    )


def build(font_path, out_dir, variant):
    # Mark geometry on a 98 x 72 canvas.
    cx, cy, r = 47.0, 58.0, 39.0
    mark_w, mark_h = 98.0, 72.0

    def mark_body(dark):
        if dark:
            return mark(cx, cy, r, WHITE, NAVY, WHITE, LIGHT_BLUE, variant, stroke=3.3)
        return mark(cx, cy, r, NAVY, WHITE, NAVY, BLUE, variant, stroke=3.3)

    files = {}
    files["nasion-mark.svg"] = svg(mark_w, mark_h, mark_body(False), "Nasion")
    files["nasion-mark-dark.svg"] = svg(mark_w, mark_h, mark_body(True), "Nasion")

    # Horizontal lockup: mark, gap, wordmark sitting on the arc baseline.
    cap_h = 34.0
    d, word_w, word_h = wordmark_path(font_path, "Nasion", cap_h, tracking=0.6)
    gap = 14.0
    word_x = mark_w + gap
    word_y = cy - cap_h + 6.0  # baseline a little below the arc ends, optically level with the dot
    total_w = word_x + word_w + 4

    def lockup(dark):
        ink = WHITE if dark else NAVY
        return (
            mark_body(dark)
            + f'\n  <path d="{d}" transform="translate({word_x:.2f} {word_y:.2f})" fill="{ink}"/>'
        )

    files["nasion-logo.svg"] = svg(total_w, mark_h, lockup(False), "Nasion")
    files["nasion-logo-dark.svg"] = svg(total_w, mark_h, lockup(True), "Nasion")

    # Favicon: arc and nasion dot only, on a navy tile, readable at 16 px.
    fav = (
        f'<rect width="64" height="64" rx="12" fill="{NAVY}"/>\n  '
        f'<path d="M13 43 A19 19 0 0 1 51 43" fill="none" stroke="{WHITE}" stroke-width="6" stroke-linecap="round"/>\n  '
        f'<circle cx="51" cy="43" r="8.5" fill="{LIGHT_BLUE}"/>'
    )
    files["favicon.svg"] = svg(64, 64, fav, "Nasion")

    for name, content in files.items():
        with open(f"{out_dir}/{name}", "w") as fh:
            fh.write(content)
    return sorted(files)


if __name__ == "__main__":
    font_path, out_dir = sys.argv[1], sys.argv[2]
    variant = sys.argv[3] if len(sys.argv) > 3 else "nodes"
    for name in build(font_path, out_dir, variant):
        print(name)
