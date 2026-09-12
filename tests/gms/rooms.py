# Lists the rooms of a GameMaker: Studio 1.4 data.win: index, name, size, speed, physics world.
import struct, sys
d = open(sys.argv[1], 'rb').read()
assert d[:4] == b'FORM'
chunks = {}
o = 8
while o < len(d):
    name = d[o:o+4].decode(); size = struct.unpack_from('<I', d, o+4)[0]
    chunks[name] = (o+8, size); o += 8 + size
def s(p):  # GMS strings: pointer to the characters, length just before
    n = struct.unpack_from('<I', d, p-4)[0]
    return d[p:p+n].decode('utf-8', 'replace')
base, _ = chunks['ROOM']
n = struct.unpack_from('<I', d, base)[0]
for i in range(n):
    r = struct.unpack_from('<I', d, base + 4 + 4*i)[0]
    name_p, cap_p, w, h, speed, persistent, color, show, code, flags, bgs, views, objs, tiles, world = struct.unpack_from('<15I', d, r)
    top, left, right, bottom = struct.unpack_from('<4I', d, r + 60)
    gx, gy, mpp = struct.unpack_from('<3f', d, r + 76)
    print(f"{i:4} {s(name_p):32} {w}x{h} speed {speed} physics {world}" + (f" gravity ({gx:.1f},{gy:.1f}) m/px {mpp:.3f}" if world else ''))
