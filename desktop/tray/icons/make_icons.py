# Draws the paste-get icons. Run: python3 make_icons.py <this directory> (needs Pillow).
import sys
from PIL import Image, ImageDraw
out = sys.argv[1]
def draw(size, bg):
    s = 4  # supersample
    W = size * s
    im = Image.new("RGBA", (W, W), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    d.rounded_rectangle([0, 0, W - 1, W - 1], radius=W // 5, fill=bg)
    # Clipboard board
    d.rounded_rectangle([W*0.25, W*0.22, W*0.75, W*0.84], radius=W//16, fill=(255, 255, 255, 255))
    # Clip
    d.rounded_rectangle([W*0.38, W*0.14, W*0.62, W*0.30], radius=W//24, fill=bg)
    d.rounded_rectangle([W*0.42, W*0.17, W*0.58, W*0.27], radius=W//40, fill=(255, 255, 255, 255))
    # Down arrow (paste)
    c = bg
    d.rectangle([W*0.46, W*0.38, W*0.54, W*0.60], fill=c)
    d.polygon([(W*0.36, W*0.58), (W*0.64, W*0.58), (W*0.50, W*0.74)], fill=c)
    return im.resize((size, size), Image.LANCZOS)
active = (15, 139, 141, 255)
paused = (128, 128, 128, 255)
draw(512, active).save(f"{out}/icon.png")
draw(256, active).save(f"{out}/icon.ico", sizes=[(16,16),(24,24),(32,32),(48,48),(64,64),(128,128),(256,256)])
draw(64, active).save(f"{out}/tray.png")
draw(64, paused).save(f"{out}/tray-paused.png")
