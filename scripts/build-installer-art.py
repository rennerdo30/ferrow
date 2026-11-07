"""
Builds the setup artwork from the app logo (crates/ferrow-gui/assets).
Run once after the logo changes: python scripts/build-installer-art.py (needs Pillow).
The outputs are committed, so packaging needs no Python.

- installer/art/ferrow.ico: classic 32-bit bitmap frames for 16-64 px (the NSIS stub and Explorer
  handle them everywhere) plus a PNG frame for 256 px. Also the app's shortcut icon.
- installer/art/welcome.bmp: the welcome/finish side image at 2x the MUI size (164x314).
- installer/art/header.bmp: the header image on the inner pages at 2x the MUI size (150x57).
"""
import io
import pathlib
import struct

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parent.parent
ASSETS = ROOT / 'crates' / 'ferrow-gui' / 'assets'
OUT = ROOT / 'installer' / 'art'
OUT.mkdir(parents=True, exist_ok=True)
LOGO = Image.open(ASSETS / 'logo-256.png').convert('RGBA')
SMALL = Image.open(ASSETS / 'logo-32.png').convert('RGBA')
FONT = 'C:/Windows/Fonts/segoeuib.ttf'
FONT_REGULAR = 'C:/Windows/Fonts/segoeui.ttf'

BG = (25, 25, 25)
INK = (242, 242, 242)
INK2 = (170, 170, 170)


def bmp_frame(img: Image.Image) -> bytes:
    """32-bit BMP icon frame: BITMAPINFOHEADER (double height), bottom-up BGRA rows, then the AND mask."""
    w, h = img.size
    px = img.load()
    rows = bytearray()
    for y in range(h - 1, -1, -1):
        for x in range(w):
            r, g, b, a = px[x, y]
            rows += bytes((b, g, r, a))
    mask_stride = ((w + 31) // 32) * 4
    mask = bytearray()
    for y in range(h - 1, -1, -1):
        row = bytearray(mask_stride)
        for x in range(w):
            if px[x, y][3] == 0:
                row[x // 8] |= 0x80 >> (x % 8)
        mask += row
    header = struct.pack('<IiiHHIIiiII', 40, w, h * 2, 1, 32, 0, len(rows) + len(mask), 0, 0, 0, 0)
    return header + bytes(rows) + bytes(mask)


def icon():
    frames = []
    for size in (16, 20, 24, 32, 40, 48, 64):
        src = SMALL if size <= 32 else LOGO
        frames.append((size, bmp_frame(src.resize((size, size), Image.LANCZOS))))
    png = io.BytesIO()
    LOGO.save(png, format='PNG')
    frames.append((256, png.getvalue()))
    head = struct.pack('<HHH', 0, 1, len(frames))
    offset = 6 + 16 * len(frames)
    entries, data = b'', b''
    for size, blob in frames:
        entries += struct.pack('<BBBBHHII', size % 256, size % 256, 0, 0, 1, 32, len(blob), offset + len(data))
        data += blob
    (OUT / 'ferrow.ico').write_bytes(head + entries + data)


def welcome():
    w, h = 328, 628
    img = Image.new('RGB', (w, h), BG)
    d = ImageDraw.Draw(img)
    logo = LOGO.resize((144, 144), Image.LANCZOS)
    img.paste(logo, ((w - 144) // 2, 150), logo)
    d.text((w // 2, 340), 'Ferrow', font=ImageFont.truetype(FONT, 44), fill=INK, anchor='mm')
    d.text((w // 2, 384), 'Find any file by name,', font=ImageFont.truetype(FONT_REGULAR, 20), fill=INK2, anchor='mm')
    d.text((w // 2, 410), 'as fast as you type.', font=ImageFont.truetype(FONT_REGULAR, 20), fill=INK2, anchor='mm')
    img.save(OUT / 'welcome.bmp')


def header():
    w, h = 300, 114
    img = Image.new('RGB', (w, h), (255, 255, 255))
    logo = LOGO.resize((76, 76), Image.LANCZOS)
    img.paste(logo, (w - 76 - 16, (h - 76) // 2), logo)
    img.save(OUT / 'header.bmp')


icon()
welcome()
header()
print('installer art written to', OUT)
