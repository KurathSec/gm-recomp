# Shaders in a GMS 1.4 data.win, and which code entries call shader_set.
import struct, sys
d = open(sys.argv[1], 'rb').read()
ch = {}; o = 8
while o < len(d):
    n = d[o:o+4].decode(); sz = struct.unpack_from('<I', d, o+4)[0]; ch[n] = (o+8, sz); o += 8 + sz
def gstr(p):
    n = struct.unpack_from('<I', d, p-4)[0]; return d[p:p+n].decode('utf-8', 'replace')
names = []
if 'SHDR' in ch:
    b, _ = ch['SHDR']; n = struct.unpack_from('<I', d, b)[0]
    names = [gstr(struct.unpack_from('<I', d, struct.unpack_from('<I', d, b+4+4*i)[0])[0]) for i in range(n)]
print('shaders:', names)
if 'CODE' not in ch or ch['CODE'][1] == 0: sys.exit()
codes = []
b, _ = ch['CODE']; n = struct.unpack_from('<I', d, b)[0]
for i in range(n):
    p = struct.unpack_from('<I', d, b+4+4*i)[0]
    np, length, locs, args, rel = struct.unpack_from('<IIHHi', d, p)
    codes.append((gstr(np), p + 12 + rel, length))
b, _ = ch['FUNC']; nf = struct.unpack_from('<I', d, b)[0]; o = b + 4
for i in range(nf):
    np, occ, first = struct.unpack_from('<IIi', d, o); o += 12
    if gstr(np) != 'shader_set': continue
    a = first; users = set()
    for _ in range(occ):
        for nm, s, l in codes:
            if s <= a < s + l: users.add(nm)
        a += struct.unpack_from('<I', d, a+4)[0] & 0x07FFFFFF
    print('shader_set in:', sorted(users))
