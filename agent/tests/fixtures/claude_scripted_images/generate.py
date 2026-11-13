#!/usr/bin/env python3
"""Writes the image fixtures the scripted scenarios send (run it again only to change them; the files are committed).

Each file's name says what it is: `<kind>_<width>x<height>[_<note>].<ext>`. A scenario names a file in its driver
part: {"send": {"text": "...", "images": ["png_48x32.png"]}}. Needs Pillow.
"""
import os
from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))


def picture(width, height, seed, alpha=False):
    mode = "RGBA" if alpha else "RGB"
    image = Image.new(mode, (width, height), (20 + seed * 40, 90, 200 - seed * 30) + ((255,) if alpha else ()))
    draw = ImageDraw.Draw(image)
    draw.rectangle([width // 4, height // 4, width * 3 // 4, height * 3 // 4], fill=(240, 200 - seed * 20, 40 + seed * 30) + ((255,) if alpha else ()))
    draw.line([0, 0, width, height], fill=(255, 255, 255) + ((255,) if alpha else ()), width=2)
    if alpha:
        draw.ellipse([0, 0, width // 3, height // 3], fill=(0, 0, 0, 0))
    return image


def save(name, image, **options):
    image.save(os.path.join(HERE, name), **options)


save("png_48x32.png", picture(48, 32, 1), optimize=False)
save("png_alpha_48x32.png", picture(48, 32, 2, alpha=True), optimize=False)
save("jpeg_48x32.jpg", picture(48, 32, 3), quality=85)
first = picture(40, 40, 4)
second = picture(40, 40, 5)
first.convert("P").save(os.path.join(HERE, "gif_40x40_two_frames.gif"), save_all=True, append_images=[second.convert("P")], duration=100, loop=0)
save("webp_48x32.webp", picture(48, 32, 6), quality=80, method=0)
