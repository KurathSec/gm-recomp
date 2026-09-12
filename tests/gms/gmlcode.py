# Reads a GameMaker: Studio 1.4 data.win (bytecode 15/16): chunk table, code entries,
# and (with a name filter) a rough disassembly with variable/function names resolved.
import struct, sys, re
d = open(sys.argv[1], 'rb').read()
ch = {}; o = 8
while o < len(d):
    n = d[o:o+4].decode(); sz = struct.unpack_from('<I', d, o+4)[0]; ch[n] = (o+8, sz); o += 8 + sz
def gstr(p):
    n = struct.unpack_from('<I', d, p-4)[0]; return d[p:p+n].decode('utf-8', 'replace')
def plist(name):
    b, _ = ch[name]; n = struct.unpack_from('<I', d, b)[0]
    return [struct.unpack_from('<I', d, b+4+4*i)[0] for i in range(n)]
# strings by index
strs = [d[p+4:p+4+struct.unpack_from('<I', d, p)[0]].decode('utf-8', 'replace') for p in plist('STRG')]
# references: VARI and FUNC link chains (address of first use, then relative links)
refs = {}
def chains(name, entry_size, hdr):
    b, sz = ch[name]; o = b + hdr; end = b + sz
    while o + entry_size <= end:
        if name == 'VARI':
            np, kind, vid, occ, first = struct.unpack_from('<IiiIi', d, o)
        else:
            np, occ, first = struct.unpack_from('<IIi', d, o)
        a = first; nm = gstr(np)
        for _ in range(occ):
            if a <= 0 or a >= len(d): break
            refs[a] = nm
            nxt = struct.unpack_from('<I', d, a+4)[0] & 0x07FFFFFF
            a += nxt
        o += entry_size
chains('VARI', 20, 12)
b, sz = ch['FUNC']; nf = struct.unpack_from('<I', d, b)[0]
o = b + 4
for i in range(nf):
    np, occ, first = struct.unpack_from('<IIi', d, o); nm = gstr(np); a = first
    for _ in range(occ):
        if a <= 0 or a >= len(d): break
        refs[a] = nm
        a += struct.unpack_from('<I', d, a+4)[0] & 0x07FFFFFF
    o += 12
codes = []
for p in plist('CODE'):
    np, length, locs, args, rel = struct.unpack_from('<IIHHi', d, p)
    start = p + 12 + rel
    codes.append((gstr(np), start, length))
pat = sys.argv[2] if len(sys.argv) > 2 else None
if not pat:
    for n, s, l in codes: print(n, l)
    sys.exit()
OPS = {0x07:'conv',0x08:'mul',0x09:'div',0x0A:'rem',0x0B:'mod',0x0C:'add',0x0D:'sub',0x0E:'and',0x0F:'or',0x10:'xor',0x11:'neg',0x12:'not',0x13:'shl',0x14:'shr',0x15:'cmp',0x45:'pop',0x84:'pushi',0x86:'dup',0x9C:'ret',0x9D:'exit',0x9E:'popz',0xB6:'b',0xB7:'bt',0xB8:'bf',0xBA:'pushenv',0xBB:'popenv',0xC0:'push',0xC1:'pushloc',0xC2:'pushglb',0xC3:'pushbltn',0xD9:'call',0xFF:'break'}
CMP = {1:'<',2:'<=',3:'==',4:'!=',5:'>=',6:'>'}
for n, s, l in codes:
    if not re.search(pat, n): continue
    print('====', n)
    a = s
    while a < s + l:
        w = struct.unpack_from('<I', d, a)[0]; op = w >> 24; t1 = (w >> 16) & 0xF; t2 = (w >> 20) & 0xF
        name = OPS.get(op, f'op{op:02x}'); txt = name; size = 4
        if op in (0xC0, 0xC1, 0xC2, 0xC3, 0x84):
            if t1 == 0: txt += f' {struct.unpack_from("<d", d, a+4)[0]}'; size = 12
            elif t1 in (2, 5): txt += f' {struct.unpack_from("<i", d, a+4)[0] if op != 0x84 else w & 0xFFFF}'; size = 8 if op != 0x84 else 4
            elif t1 == 3: txt += f' {struct.unpack_from("<q", d, a+4)[0]}'; size = 12
            elif t1 == 6: txt += f' "{strs[struct.unpack_from("<I", d, a+4)[0]]}"'; size = 8
            elif t1 == 0xF: txt += f' {w & 0xFFFF}'
            elif t1 == 7: txt += f' var:{refs.get(a, "?")}'; size = 8
            if op == 0x84: size = 4; txt = f'pushi {(w & 0xFFFF) - (0x10000 if w & 0x8000 else 0)}'
        elif op == 0x45: txt += f' var:{refs.get(a, "?")}'; size = 8
        elif op == 0xD9: txt += f' {refs.get(a, "?")}({w & 0xFFFF})'; size = 8
        elif op in (0xB6, 0xB7, 0xB8): off = w & 0x7FFFFF; off = off - 0x800000 if off & 0x400000 else off; txt += f' {(a - s + off*4):x}'
        elif op == 0x15: txt += f' {CMP.get((w >> 8) & 0xFF, "?")}'
        print(f'  {a - s:5x} {txt}')
        a += size
