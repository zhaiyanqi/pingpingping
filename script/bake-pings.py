"""Bake lolPing's overlay.css ping keyframes into transparent, single-play WebPs."""
from pathlib import Path

import numpy as np
from PIL import Image, ImageColor, ImageFilter

ASSETS = Path(__file__).resolve().parents[1] / "assets"
DPI = 1.5
SIDE = 256
ART = 110
Y, X = (np.indices((int(SIDE * DPI),) * 2) + 0.5) / DPI - SIDE / 2
RADIUS = np.hypot(X, Y)
PINGS = [
    ("pingwheel_retreatrender", "#ff3b55"),
    ("pingwheel_pushrender", "#35d27a"),
    ("omw_cyan", "#2e8bff"),
    ("png_all_in", "#f2b320"),
    ("png_assist_me_128x128", "#35d27a"),
    ("mt11_18_vision", "#35d27a"),
    ("png_enemymissing_128x128", "#f2c518"),
    ("pingwheel_enemyvisionrender", "#ff3b55"),
    ("pingwheel_basicpingrender", "#2e8bff"),
]


def bezier(t, x1, y1, x2, y2):
    def coordinate(u, a, b):
        return 3 * (1-u)**2*u*a + 3*(1-u)*u*u*b + u**3
    lo, hi = 0, 1
    for _ in range(18):
        mid = (lo + hi) / 2
        if coordinate(mid, x1, x2) < t:
            lo = mid
        else:
            hi = mid
    return coordinate((lo + hi) / 2, y1, y2)


def ease(t):
    return bezier(np.clip(t, 0, 1), 0, 0, .58, 1)


def tint(color, alpha):
    layer = Image.new("RGBA", X.shape, color)
    layer.putalpha(Image.fromarray((255 * np.clip(alpha, 0, 1)).astype("uint8")))
    return layer


def texture(name, color):
    edge = round(ART * DPI)
    art = Image.open(ASSETS / "textures" / f"{name}.png").convert("RGBA").resize((edge, edge), Image.Resampling.LANCZOS)
    # Pad before blurring so the glow extends outside the artwork.
    # 先留出边距，再模糊透明通道，让辉光延伸到图案之外。
    pad = round(30 * DPI)
    image = Image.new("RGBA", (edge + 2 * pad,) * 2)
    image.alpha_composite(art, (pad, pad))
    glow = Image.new("RGBA", image.size, color)
    glow.putalpha(image.getchannel("A").filter(ImageFilter.GaussianBlur(10 * DPI)))
    return Image.alpha_composite(glow, image)


def frame(t, color, sprites):
    life = np.clip((3.2 - t) / (.13 * 3.2), 0, 1)
    ground = (.75 * ease(t/.32) if t < .32 else
              .75 - .2 * ease((t-.32)/2.4) if t < 2.72 else .55 * (1-ease((t-2.72)/.48)))
    image = tint(color, ground * (1 - np.hypot(X/(.42*ART), (Y-.32*ART)/(.12*ART))))
    for delay in (0, .22, 1.4):
        if delay <= t < delay + .65:
            p = ease((t-delay)/.65)
            scale = .25 + 1.1*p
            dist = RADIUS/scale - .545*ART
            shadow = np.where(dist > 0, np.exp(-.5*(dist/9)**2), np.exp(-.5*((dist+3)/6)**2))
            border = np.clip(np.minimum(-dist, dist+3) * DPI * scale + .5, 0, 1)
            image.alpha_composite(tint(color, .95*(1-p)*np.maximum(border, .52*shadow)))
    clash = len(sprites) == 2
    bob = max(0, t - (.5 if clash else .3)) % 1.6
    y = -.23*ART - 5*bezier(1-abs(bob/.8-1), .42, 0, .58, 1)
    for i, art in enumerate(sprites):
        x, dy, scale, opacity = 0, 0, 1, life
        if clash:
            p = bezier(min(1, t/.22), .5, 0, .9, .5)
            shake = np.clip((t-.22)/.25, 0, 1)*4
            x = (1 if i == 0 else -1)*.64*ART*(1-p) + np.interp(shake, range(5), [0,-3,3,-2,0])
            dy = -.64*ART*(1-p) + np.interp(shake, range(5), [0,2,-2,-1,0])
            opacity *= p
        elif t < .28:
            first = t < .168
            p = bezier(t/.168 if first else (t-.168)/.112, .2, 1.6, .4, 1)
            scale = .4+.75*p if first else 1.15-.15*p
            opacity *= min(1, p) if first else 1
        edge = round(art.width * scale)
        tile = art.resize((edge, edge), Image.Resampling.BILINEAR)
        tile.putalpha(tile.getchannel("A").point(lambda a: round(a*opacity)))
        image.alpha_composite(tile, (round((SIDE/2+x)*DPI-edge/2), round((SIDE/2+y+dy)*DPI-edge/2)))
    if clash and .2 <= t < .55:
        u = (t-.2)/.35
        alpha = ease(u/.3) if u < .3 else 1-ease((u-.3)/.7)
        radius = .25*ART*(.3+1.9*ease(u))
        r = np.hypot(X, Y+.23*ART)/radius
        flash = tint(color, alpha*np.clip((1-r)/.6, 0, 1))
        flash.alpha_composite(tint("white", alpha*np.clip(1-r/.4, 0, 1)))
        image.alpha_composite(flash)
    return image


def bake(item):
    name, color = item
    names = ["pingwheel_allin_render_01", "pingwheel_allin_render_02"] if name == "png_all_in" else [name]
    sprites = [texture(n, ImageColor.getrgb(color)) for n in names]
    frames = [frame(ms/1000, color, sprites) for ms in range(0, 3200, 20)]
    frames[0].save(ASSETS / "ping-fx" / f"{name}.webp", save_all=True, append_images=frames[1:],
                   duration=20, loop=1, quality=88, method=2)


if __name__ == "__main__":
    from concurrent.futures import ProcessPoolExecutor
    with ProcessPoolExecutor(max_workers=3) as pool:
        list(pool.map(bake, PINGS))
