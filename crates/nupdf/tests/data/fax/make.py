# CCITT fax data as libtiff (through Pillow) encodes it, one strip each.
from PIL import Image, TiffImagePlugin

w, h = 45, 12
im = Image.new("1", (w, h), 1)
px = im.load()
for y in range(h):
    for x in range(w):
        if (x // (y + 2)) % 2 == 0 or x == y * 3:
            px[x, y] = 0
for name, comp, t4 in [
    ("g3_1d", "group3", 0),
    ("g3_1d_aligned", "group3", 4),
    ("g3_2d", "group3", 1),
    ("g3_2d_aligned", "group3", 5),
    ("g4", "group4", None),
]:
    info = TiffImagePlugin.ImageFileDirectory_v2()
    if t4 is not None:
        info[292] = t4
    im.save(name + ".tif", compression=comp, tiffinfo=info)
    t = Image.open(name + ".tif")
    off = t.tag_v2[273][0]
    n = t.tag_v2[279][0]
    data = open(name + ".tif", "rb").read()[off:off + n]
    open(name + ".fax", "wb").write(data)
    print(name, len(data), t.tag_v2.get(292), t.tag_v2.get(262))
