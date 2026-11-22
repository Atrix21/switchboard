"""Assemble captured PNG frames into the README demo GIF.
usage: python scripts/make_gif.py <frames_dir> <out.gif> [width] [fps] [crop_right_px]"""
import glob, sys
from PIL import Image

src, out = sys.argv[1], sys.argv[2]
width = int(sys.argv[3]) if len(sys.argv) > 3 else 960
fps = int(sys.argv[4]) if len(sys.argv) > 4 else 10
crop_right = int(sys.argv[5]) if len(sys.argv) > 5 else 0
frames = []
for f in sorted(glob.glob(f"{src}/*.png")):
    im = Image.open(f).convert("RGB")
    if crop_right:
        im = im.crop((0, 0, im.width - crop_right, im.height))
    h = round(im.height * width / im.width)
    frames.append(im.resize((width, h), Image.LANCZOS))
# One shared palette keeps the file small and avoids colour flicker between frames.
pal = frames[len(frames) // 2].quantize(colors=128, method=Image.Quantize.MEDIANCUT)
q = [f.quantize(palette=pal, dither=Image.Dither.NONE) for f in frames]
q[0].save(out, save_all=True, append_images=q[1:], duration=int(1000 / fps), loop=0, optimize=True, disposal=1)
print(f"{len(q)} frames -> {out}")
