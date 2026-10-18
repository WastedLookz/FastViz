#!/usr/bin/env python3
"""Writes one image per tricky case into the given folder (see make-test-media.sh)."""
import os, sys, random
from PIL import Image, ImageDraw, features
out = sys.argv[1]
def scene(w, h, mode="RGB", label=""):
    img = Image.new("RGB", (w, h))
    d = ImageDraw.Draw(img)
    for y in range(0, h, max(1, h // 64)):
        d.rectangle([0, y, w, y + h // 64], fill=(int(255 * y / h), 120, 255 - int(255 * y / h)))
    d.ellipse([w * 0.2, h * 0.2, w * 0.8, h * 0.8], fill=(250, 220, 40))
    # An arrow pointing "up" so orientation mistakes are obvious.
    d.polygon([(w * 0.5, h * 0.25), (w * 0.35, h * 0.5), (w * 0.65, h * 0.5)], fill=(20, 20, 20))
    d.rectangle([w * 0.45, h * 0.5, w * 0.55, h * 0.75], fill=(20, 20, 20))
    return img.convert(mode) if mode != "RGB" else img
p = lambda n: os.path.join(out, n)

# JPEG stored sideways with EXIF orientation 6 (rotate 90 CW to view) and a date taken.
upright = scene(3000, 4000)
stored = upright.rotate(90, expand=True)          # 4000x3000, arrow pointing left
exif = Image.Exif(); exif[0x0112] = 6
exif.get_ifd(0x8769)[0x9003] = "2021:07:04 09:15:00"
stored.save(p("rotated_exif6.jpg"), quality=90, exif=exif.tobytes())
scene(4000, 3000).save(p("progressive.jpg"), quality=85, progressive=True)
scene(1200, 900).convert("CMYK").save(p("cmyk.jpg"), quality=90)
rgba = scene(800, 600).convert("RGBA"); a = Image.new("L", rgba.size, 0)
ImageDraw.Draw(a).ellipse([100, 50, 700, 550], fill=255); rgba.putalpha(a)
rgba.save(p("alpha.png"))
scene(1000, 700).convert("L").convert("I").point(lambda v: v * 256).convert("I;16").save(p("gray16.png"))
scene(640, 480).convert("P", palette=Image.ADAPTIVE).save(p("palette.png"))
if features.check("webp"):
    scene(1600, 1000).save(p("lossy.webp"), quality=80)
    rgba.save(p("alpha.webp"), lossless=True)
frames = [scene(300, 200).rotate(i * 30) for i in range(6)]
frames[0].save(p("animated.gif"), save_all=True, append_images=frames[1:], duration=200, loop=0)
scene(500, 400).save(p("plain.bmp"))
scene(1500, 1000).save(p("lzw.tif"), compression="tiff_lzw")
scene(16, 16).save(p("tiny.png"))
scene(8000, 8000).save(p("huge.png"), compress_level=1)
# Broken and mislabeled files.
data = open(p("progressive.jpg"), "rb").read()
open(p("truncated.jpg"), "wb").write(data[: len(data) // 3])
open(p("garbage.jpg"), "wb").write(bytes(random.getrandbits(8) for _ in range(5000)))
open(p("empty.png"), "wb").close()
open(p("fake.jxl"), "wb").write(b"\xff\x0a" + bytes(100))
open(p("png_named.jpg"), "wb").write(open(p("palette.png"), "rb").read())
open(p("no_extension"), "wb").write(open(p("progressive.jpg"), "rb").read())
print("webp:", features.check("webp"))
