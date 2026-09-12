# Makes a test copy of an imported GMS game with a patched data.win (files symlinked),
# to reach a situation for a check without playing there:
#   testcopy.py <game dir> <out dir> speed <room name> <speed>
#   testcopy.py <game dir> <out dir> swap <room a> <room b>   (exchange two ROOM entries)
#   testcopy.py <game dir> <out dir> word <code entry> <offset> <old> <new> [...]
#     replaces bytecode words (offsets from the entry's start; old is checked). Keep
#     variable/function reference instructions intact (they are chained at load):
#     change constants, or branch over an instruction (b +n = 0xB6000000 | n words).
# Recipes used for docs/r3-r5-runtime.md:
#   CN3, honour the rooms' own speeds (skip objWorld's room_speed = 50):
#     word gml_Object_objWorld_Other_4 0x28 0x840f0032 0xb6000003
#   Marathon, start in rGolfWond (startRoom 15 -> 55; skip the golf autosave that
#   needs state from earlier stages):
#     word gml_Script_scrSetGlobalOptions 0x8c 0x840f000f 0x840f0037 \
#          gml_Object_oGolfStart_Create_0 0x64 0x840f0001 0xb6000005
import json, os, shutil, struct, sys
src, dst, op = sys.argv[1], sys.argv[2], sys.argv[3]
shutil.rmtree(dst, ignore_errors=True)
os.makedirs(os.path.join(dst, 'files'))
shutil.copy(os.path.join(src, 'manifest.json'), dst)
for name in os.listdir(os.path.join(src, 'files')):
    if name != 'data.win':
        os.symlink(os.path.abspath(os.path.join(src, 'files', name)), os.path.join(dst, 'files', name))
d = bytearray(open(os.path.join(src, 'files', 'data.win'), 'rb').read())
ch = {}; o = 8
while o < len(d):
    n = d[o:o+4].decode(); sz = struct.unpack_from('<I', d, o+4)[0]; ch[n] = (o+8, sz); o += 8 + sz
def gstr(p):
    n = struct.unpack_from('<I', d, p-4)[0]; return d[p:p+n].decode()
b, _ = ch['ROOM']; n = struct.unpack_from('<I', d, b)[0]
slot = {gstr(struct.unpack_from('<I', d, struct.unpack_from('<I', d, b+4+4*i)[0])[0]): b+4+4*i for i in range(n)}
if op == 'speed':
    r = struct.unpack_from('<I', d, slot[sys.argv[4]])[0]
    struct.pack_into('<I', d, r + 16, int(sys.argv[5]))
elif op == 'swap':
    a, c = slot[sys.argv[4]], slot[sys.argv[5]]
    ra, rc = struct.unpack_from('<I', d, a)[0], struct.unpack_from('<I', d, c)[0]
    struct.pack_into('<I', d, a, rc); struct.pack_into('<I', d, c, ra)
elif op == 'word':
    b, _ = ch['CODE']; n = struct.unpack_from('<I', d, b)[0]
    entries = {}
    for i in range(n):
        e = struct.unpack_from('<I', d, b+4+4*i)[0]
        np, length, locs, args, rel = struct.unpack_from('<IIHHi', d, e)
        entries[gstr(np)] = e + 12 + rel
    rest = sys.argv[4:]
    for k in range(0, len(rest), 4):
        name, off, old, new = rest[k], int(rest[k+1], 0), int(rest[k+2], 0), int(rest[k+3], 0)
        a = entries[name] + off
        have = struct.unpack_from('<I', d, a)[0]
        assert have == old, f'{name}+{off:#x}: {have:#x}, expected {old:#x}'
        struct.pack_into('<I', d, a, new)
open(os.path.join(dst, 'files', 'data.win'), 'wb').write(d)
print('wrote', dst)
