"""Evaluate one pixel assertion on a screenshot (used by test-headless-sway.sh).

usage: shotcheck.py <png> <output logical width> <python expression>

The expression sees, in logical coordinates of the output:
  px(x, y)            colour at a logical point
  bg(p, t=24)         p is the compositor's empty-output colour (within t)
  all_bg(t=24)        every pixel is the empty-output colour: nothing of ours
  clay(p) white(p) reddish(p)
  region(x0, y0, x1, y1, pred)   number of pixels matching pred
The empty-output colour comes from SHOT_BG="r,g,b" (a screenshot taken before
any overlay existed); it is black on sway >= 1.10 and dark gray on older sway.
Exit 0 when the expression is true.
"""

import os
import sys

from PIL import Image, ImageChops

im = Image.open(sys.argv[1]).convert("RGB")
k = im.width / float(sys.argv[2])  # image px per logical px
BG = tuple(int(v) for v in os.environ.get("SHOT_BG", "0,0,0").split(","))


def px(x, y):
    return im.getpixel((min(im.width - 1, int(x * k)), min(im.height - 1, int(y * k))))


def clay(p):
    return p[0] > 150 and 70 < p[1] < 160 and p[2] < 130 and p[0] - p[2] > 70


def white(p):
    return min(p) > 200


def bg(p, t=24):
    return max(abs(a - b) for a, b in zip(p, BG)) <= t


def reddish(p):
    return p[0] - p[2] > 25 and p[0] > 40 and not bg(p, 8)


def region(x0, y0, x1, y1, pred):
    return sum(
        1
        for y in range(int(y0 * k), int(y1 * k))
        for x in range(int(x0 * k), int(x1 * k))
        if pred(im.getpixel((x, y)))
    )


def all_bg(t=24):
    diff = ImageChops.difference(im, Image.new("RGB", im.size, BG))
    return max(hi for _, hi in diff.getextrema()) <= t


sys.exit(0 if eval(sys.argv[3]) else 1)
