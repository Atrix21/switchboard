"""Draws the Switchboard mark (two patch jacks joined by a cable) as PNGs for docs.
Same geometry as ui/src/icon.rs, on a rounded graphite tile."""
from PIL import Image, ImageDraw

def mark(size):
    S = 4 * size  # supersample
    im = Image.new("RGBA", (S, S), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    r = S * 0.22
    d.rounded_rectangle((0, 0, S - 1, S - 1), radius=r, fill=(24, 32, 43, 255))
    brass, teal, coral = (194, 154, 74, 255), (61, 178, 164, 255), (224, 100, 90, 255)
    ax, ay, bx, by = S * 0.30, S * 0.62, S * 0.70, S * 0.38
    jr, w = S * 0.13, int(S * 0.05)
    # cable: a bowed curve sampled as a polyline
    pts = []
    for i in range(41):
        t = i / 40
        x = ax + (bx - ax) * t
        y = ay + (by - ay) * t + t * (1 - t) * S * 0.30
        pts.append((x, y))
    d.line(pts, fill=brass, width=w, joint="curve")
    for (cx, cy, col) in ((ax, ay, teal), (bx, by, coral)):
        d.ellipse((cx - jr, cy - jr, cx + jr, cy + jr), fill=(24, 32, 43, 255), outline=col, width=w)
        pr = jr * 0.36
        d.ellipse((cx - pr, cy - pr, cx + pr, cy + pr), fill=col)
    return im.resize((size, size), Image.LANCZOS)

for s in (512, 128):
    mark(s).save(f"docs/switchboard-{s}.png")
print("ok")
