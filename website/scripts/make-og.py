"""Render public/og.png, the 1200x630 social preview card.

Run from website/:  python3 scripts/make-og.py   (needs Pillow)
Uses the Nationale font from crates/tide-app/fonts when present, else Helvetica Neue.
"""

from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter, ImageFont

ROOT = Path(__file__).resolve().parent.parent
W, H = 1200, 630
OUT = ROOT / "public" / "og.png"
LOGO = ROOT / "src" / "assets" / "tide.png"
SHOT = ROOT / "src" / "assets" / "screenshots" / "app-home.png"
FONT_DIR = ROOT.parent / "crates" / "tide-app" / "fonts"


def font(weight: str, size: int) -> ImageFont.FreeTypeFont:
    nationale = FONT_DIR / f"Nationale-{weight}.otf"
    if nationale.exists():
        return ImageFont.truetype(str(nationale), size)
    index = {"Bold": 1, "DemiBold": 10, "Regular": 0}.get(weight, 0)
    return ImageFont.truetype("/System/Library/Fonts/HelveticaNeue.ttc", size, index=index)


def rounded(img: Image.Image, radius: int) -> Image.Image:
    mask = Image.new("L", img.size, 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, *img.size), radius, fill=255)
    out = img.convert("RGBA")
    out.putalpha(mask)
    return out


card = Image.new("RGBA", (W, H), (0, 0, 0, 255))

# Warm glow behind the screenshot, echoing the site's hero.
glow = Image.new("RGBA", (W, H), (0, 0, 0, 0))
ImageDraw.Draw(glow).ellipse((620, 120, 1300, 640), fill=(255, 212, 50, 34))
card.alpha_composite(glow.filter(ImageFilter.GaussianBlur(90)))

# App screenshot, bleeding off the right edge.
shot = Image.open(SHOT).convert("RGBA").convert("RGB")
shot_w = 760
shot = shot.resize((shot_w, round(shot.height * shot_w / shot.width)), Image.LANCZOS)
shot = rounded(shot, 14)
border = Image.new("RGBA", (shot.width + 2, shot.height + 2), (0, 0, 0, 0))
ImageDraw.Draw(border).rounded_rectangle((0, 0, shot.width + 1, shot.height + 1), 15, outline=(42, 42, 42, 255))
sx, sy = 630, 150
card.alpha_composite(border, (sx - 1, sy - 1))
card.alpha_composite(shot, (sx, sy))

# Copy.
d = ImageDraw.Draw(card)
logo = Image.open(LOGO).convert("RGBA").resize((84, 84), Image.LANCZOS)
card.alpha_composite(logo, (64, 70))
d.text((164, 94), "TIDE", font=font("Bold", 40), fill=(255, 255, 255))

head = font("Bold", 58)
y = 210
for line in ("The native player", "for your TIDAL", "subscription."):
    d.text((64, y), line, font=head, fill=(255, 255, 255))
    y += 66

sub = font("Regular", 24)
d.text((64, y + 28), "Pure Rust for macOS. Bit-perfect,", font=sub, fill=(160, 160, 160))
d.text((64, y + 60), "parametric EQ, no telemetry.", font=sub, fill=(160, 160, 160))

meta = font("DemiBold", 18)
x = 64
for label, color in (("MAX", (255, 212, 50)), ("HIGH", (51, 255, 238)), ("FREE · OPEN SOURCE", (144, 144, 144))):
    d.text((x, 560), label, font=meta, fill=color)
    x += d.textlength(label, font=meta) + 22

card.convert("RGB").save(OUT, optimize=True)
print(f"wrote {OUT.relative_to(ROOT)} ({OUT.stat().st_size // 1024} KB)")
