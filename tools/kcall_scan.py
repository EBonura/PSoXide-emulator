#!/usr/bin/env python3
"""Static PS1 kernel-call audit of a disc image.

Walks the ISO9660 filesystem of a PS1 disc (.cue/.bin, .ccd/.img, plain
.bin/.iso, or .img.ecm), finds the boot EXE named in SYSTEM.CNF plus every
other file, and reports, per file:

* BIOS dispatch stubs: a register loaded with 0xA0/0xB0/0xC0 (addiu/ori from
  $zero, or lui 0x8000/0xA000 plus addiu/ori) followed by `jr`/`jalr` on it,
  or a direct `j`/`jal` to 0xA0/0xB0/0xC0, with the function number taken
  from an `addiu/ori $t1,$zero,N` in the delay slot or the few instructions
  before;
* SYSCALL instructions and the `$a0` code loaded just before them;
* absolute kernel-RAM loads/stores (base $zero, or a base register loaded via
  lui 0x8000/0xA000 a few instructions earlier) with the target address;
* for B(56h) GetC0Table / B(57h) GetB0Table call sites, the OpenBIOS-style
  patch signature hash (hash algorithm and masks from pcsx-redux
  src/mips/openbios/patches, MIT) of the 16 words at the return address, and
  the matching known patch name when there is one.

Output is facts only (file names, offsets, function numbers, hashes); no game
bytes or disassembly are emitted. Run it on your own discs and keep the JSON
local if it names your files.

The patch signature hash, its per-table masks and the known-variant values
come from pcsx-redux src/mips/openbios/patches (MIT License, Copyright (c)
2021 PCSX-Redux authors).

usage: kcall_scan.py <disc.cue|.ccd|.bin|.iso|.img.ecm> [--json out.json]
"""

import hashlib
import json
import os
import re
import struct
import sys

SECTOR_RAW = 2352

# ---------------------------------------------------------------- disc input


def ecm_decode(path):
    """Decode an .ecm container into a raw 2352-byte-sector image (bytes).

    Only user data and subheaders matter for this tool, so EDC/ECC fields of
    regenerated sectors are left zero.
    """
    out = bytearray()
    with open(path, "rb") as f:
        data = f.read()
    if data[:4] != b"ECM\x00":
        raise ValueError("not an ECM file")
    p = 4
    sync = b"\x00" + b"\xff" * 10 + b"\x00"
    while True:
        c = data[p]
        p += 1
        typ = c & 3
        num = (c >> 2) & 0x1F
        bits = 5
        while c & 0x80:
            c = data[p]
            p += 1
            num |= (c & 0x7F) << bits
            bits += 7
        if num == 0xFFFFFFFF:
            break
        num += 1
        if typ == 0:
            out += data[p : p + num]
            p += num
            continue
        for _ in range(num):
            if typ == 1:
                # Full Mode 1 sector: sync, 3-byte address, mode, data.
                sec = bytearray(SECTOR_RAW)
                sec[0:12] = sync
                sec[12:15] = data[p : p + 3]
                sec[15] = 1
                sec[16 : 16 + 2048] = data[p + 3 : p + 3 + 2048]
                p += 3 + 2048
            elif typ == 2:
                # Mode 2 Form 1 body only (2336 bytes); the 16-byte
                # sync/header of a 2352 image arrives as a raw chunk.
                sec = bytearray(2336)
                sec[0:4] = data[p : p + 4]
                sec[4:8] = data[p : p + 4]
                sec[8 : 8 + 2048] = data[p + 4 : p + 4 + 2048]
                p += 4 + 2048
            else:
                sec = bytearray(2336)
                sec[0:4] = data[p : p + 4]
                sec[4:8] = data[p : p + 4]
                sec[8 : 8 + 2324] = data[p + 4 : p + 4 + 2324]
                p += 4 + 2324
            out += sec
    # ECM ends with a 4-byte EDC of the whole image; ignore.
    return bytes(out)


class Disc:
    def __init__(self, path):
        self.path = path
        self.image_path, self.sector_size, self.image = self._open(path)

    def _open(self, path):
        low = path.lower()
        base = os.path.dirname(path)
        if low.endswith(".cue"):
            text = open(path, encoding="latin-1").read()
            m = re.search(r'FILE\s+"([^"]+)"', text) or re.search(r"FILE\s+(\S+)", text)
            img = os.path.join(base, m.group(1))
            mode = re.search(r"TRACK\s+01\s+(\S+)", text)
            size = 2048 if mode and mode.group(1).upper().endswith("/2048") else SECTOR_RAW
            return img, size, open(img, "rb").read()
        if low.endswith(".ccd"):
            stem = path[:-4]
            for ext in (".img", ".bin"):
                if os.path.exists(stem + ext):
                    return stem + ext, SECTOR_RAW, open(stem + ext, "rb").read()
            if os.path.exists(stem + ".img.ecm"):
                return stem + ".img.ecm", SECTOR_RAW, ecm_decode(stem + ".img.ecm")
            raise FileNotFoundError(stem + ".img")
        if low.endswith(".ecm"):
            return path, SECTOR_RAW, ecm_decode(path)
        data = open(path, "rb").read()
        size = 2048 if low.endswith(".iso") else SECTOR_RAW
        return path, size, data

    def raw_sector(self, lba):
        o = lba * self.sector_size
        return self.image[o : o + self.sector_size]

    def user(self, lba):
        """(data, is_form2) for one sector."""
        s = self.raw_sector(lba)
        if len(s) < self.sector_size:
            raise EOFError("sector %d is outside the data-track image" % lba)
        if self.sector_size == 2048:
            return s, False
        mode = s[15]
        if mode == 1:
            return s[16 : 16 + 2048], False
        if s[18] & 0x20:
            return s[24 : 24 + 2324], True
        return s[24 : 24 + 2048], False

    def read_file(self, lba, size):
        chunks = []
        form2 = 0
        n = (size + 2047) // 2048
        for i in range(n):
            d, f2 = self.user(lba + i)
            form2 += f2
            chunks.append(d[:2048])
        return b"".join(chunks)[:size], form2, n

    def license_text(self):
        d, _ = self.user(4)
        t = d.decode("latin-1")
        m = re.search(r"Licensed\s+by\s+Sony\s+Computer\s+Entertainment\s*(\w+)", t)
        return m.group(1) if m else None

    def walk(self):
        pvd, _ = self.user(16)
        if pvd[1:6] != b"CD001":
            raise ValueError("no ISO9660 PVD")
        root = pvd[156 : 156 + 34]
        out = []
        seen = set()

        def rec(lba, size, prefix):
            if lba in seen:
                return
            seen.add(lba)
            data, _, _ = self.read_file(lba, size)
            p = 0
            while p < len(data):
                ln = data[p]
                if ln == 0:
                    p = (p // 2048 + 1) * 2048
                    continue
                r = data[p : p + ln]
                ext = struct.unpack_from("<I", r, 2)[0]
                dl = struct.unpack_from("<I", r, 10)[0]
                flags = r[25]
                nl = r[32]
                name = r[33 : 33 + nl]
                p += ln
                if name in (b"\x00", b"\x01"):
                    continue
                nm = name.decode("latin-1")
                if flags & 2:
                    rec(ext, dl, prefix + nm + "/")
                else:
                    out.append((prefix + nm, ext, dl))

        rec(struct.unpack_from("<I", root, 2)[0], struct.unpack_from("<I", root, 10)[0], "")
        return out


# ------------------------------------------------------------- MIPS decoding

T1 = 9
A0 = 4
VECTORS = {0xA0: "A", 0xB0: "B", 0xC0: "C"}


def words(buf):
    n = len(buf) // 4
    return struct.unpack_from("<%dI" % n, buf, 0)


def const_load(w):
    """(rt, value) for addiu/ori rt,$zero,imm; else None."""
    op = w >> 26
    rs = (w >> 21) & 31
    if rs != 0 or op not in (0x09, 0x0D):
        return None
    imm = w & 0xFFFF
    if op == 0x09 and imm & 0x8000:
        imm |= 0xFFFF0000
    return (w >> 16) & 31, imm


def reg_load_before(ws, i, reg, lookback):
    """(value, index) of the constant loaded into `reg` in ws[i-lookback..i-1].

    Understands addiu/ori from $zero and lui (+ addiu/ori on the same reg).
    The last write in the window wins; returns (None, None) if none.
    """
    found, where, hi = None, None, None
    for k in range(max(0, i - lookback), i):
        w = ws[k]
        op = w >> 26
        rt = (w >> 16) & 31
        rs = (w >> 21) & 31
        if op == 0x0F and rt == reg:
            hi = (w & 0xFFFF) << 16
            found, where = hi, k
            continue
        c = const_load(w)
        if c and c[0] == reg:
            found, where, hi = c[1], k, None
            continue
        if op in (0x09, 0x0D) and rt == reg and rs == reg and hi is not None:
            imm = w & 0xFFFF
            if op == 0x09 and imm & 0x8000:
                imm |= 0xFFFF0000
            found = (hi + imm) & 0xFFFFFFFF
            continue
    return found, where


def reg_value_before(ws, i, reg, lookback):
    return reg_load_before(ws, i, reg, lookback)[0]


def t1_near(ws, i, lookback=4):
    """Function number from the delay slot (i+1) or the instructions before."""
    if i + 1 < len(ws):
        c = const_load(ws[i + 1])
        if c and c[0] == T1:
            return c[1]
    return reg_value_before(ws, i, T1, lookback)


# OpenBIOS patch-signature hash (pcsx-redux src/mips/openbios/patches/hash.c).
HASH_MASK = {"B": 0xFFC9A655, "C": 0x5AA45555}
KNOWN_PATCHES = {
    "B": {
        0x5123F82A: "_patch_card_info#1", 0x0BC81000: "_patch_card2#1", 0xC29DF18F: "_patch_card2#2",
        0xF803A6A6: "_patch_pad#1", 0x6DEE1051: "_patch_pad#2", 0x012AFC0A: "_patch_pad#3",
        0xCEF165BA: "_remove_ChgclrPAD#1", 0x5DF8CC5D: "_remove_ChgclrPAD#2",
        0xA1C49B0E: "_send_pad#1", 0x561B6AD1: "_send_pad#2",
    },
    "C": {
        0x95C14C17: "_clear_card#1", 0xF80AEEE3: "custom_handler#1", 0x5753F599: "_initgun#1",
        0x847EABF2: "_patch_card#1", 0x2A81BBEF: "_patch_card#2", 0x61C914A1: "_patch_gte#1",
        0xC223044D: "_patch_gte#2", 0xBF873C49: "_patch_gte#3",
    },
}


def _hashone(a):
    a = ((a ^ 61) ^ (a >> 16)) & 0xFFFFFFFF
    a = (a + (a << 3)) & 0xFFFFFFFF
    a ^= a >> 4
    a = (a * 0x27D4EB2F) & 0xFFFFFFFF
    a ^= a >> 15
    return a


def patch_hash(ws16, table):
    mask_bytes = struct.pack("<I", HASH_MASK[table])
    h = 0x5810D659
    mask = 1
    mi = 0
    for n in ws16:
        if mask == 1:
            mask = mask_bytes[mi] | 0x100
            mi += 1
        m = mask & 3
        if m == 1:
            n &= 0xFFFF0000
        elif m == 2:
            n &= 0xFC000000
        elif m == 3:
            n = 0
        mask >>= 2
        h = (h + _hashone(n)) & 0xFFFFFFFF
        h = (h * 0xB503198F) & 0xFFFFFFFF
    return h


def patch_sig(ws, ra_index, fn):
    # B(56h) GetC0Table -> C0 patches; B(57h) GetB0Table -> B0 patches.
    table = "C" if fn == 0x56 else "B"
    seg = ws[ra_index : ra_index + 16]
    if len(seg) < 16:
        return None
    h = patch_hash(seg, table)
    return {"table_patched": table, "hash": "%08x" % h, "known": KNOWN_PATCHES[table].get(h)}


ANCHOR_REGS = list(range(8, 16)) + [24, 25]


def anchors(buf):
    """Word indices of candidate call instructions (fast byte search)."""
    pats = {}
    for r in ANCHOR_REGS:
        pats[struct.pack("<I", (r << 21) | 0x08)] = ("jr", r)
        pats[struct.pack("<I", (r << 21) | (31 << 11) | 0x09)] = ("jalr", r)
    for v in VECTORS:
        pats[struct.pack("<I", (0x02 << 26) | (v >> 2))] = ("j", v)
        pats[struct.pack("<I", (0x03 << 26) | (v >> 2))] = ("jal", v)
    pats[struct.pack("<I", 0x0000000C)] = ("syscall", None)
    hits = []
    for pat, kind in pats.items():
        start = 0
        while True:
            k = buf.find(pat, start)
            if k < 0:
                break
            if k % 4 == 0:
                hits.append((k // 4, kind))
            start = k + 1
    hits.sort()
    return hits


def scan_calls(buf, exe_base=None):
    """Kernel-call stubs, syscalls and patch signatures in one file buffer."""
    ws = words(buf)
    calls = []
    syscalls = []
    unresolved = 0
    for i, (kind, arg) in anchors(buf):
        if kind == "syscall":
            a0 = reg_value_before(ws, i, A0, 3)
            syscalls.append({"off": i * 4, "a0": None if a0 is None else a0})
            continue
        if kind in ("jr", "jalr"):
            v = reg_value_before(ws, i, arg, 4)
            if v is None or (v & 0x1FFFFFFF) not in VECTORS:
                continue
            table = VECTORS[v & 0x1FFFFFFF]
            form = "%s $%d (%s)" % (kind, arg, "lui" if v & 0xE0000000 else "zero")
        else:
            table = VECTORS[arg]
            form = kind
        fn = t1_near(ws, i)
        rec = {"off": i * 4, "table": table, "fn": fn, "form": form}
        if fn is None:
            unresolved += 1
        calls.append(rec)
        # Patch signatures for GetC0Table / GetB0Table.
        if table == "B" and fn in (0x56, 0x57):
            sigs = []
            if kind in ("jalr", "jal"):
                s = patch_sig(ws, i + 2, fn)
                if s:
                    s["ra_off"] = (i + 2) * 4
                    sigs.append(s)
            elif exe_base is not None:
                # jr stub: find jal callers of the stub start in this EXE.
                _, vi = reg_load_before(ws, i, arg, 4)
                _, ti = reg_load_before(ws, i, T1, 4)
                stub_start = min(x for x in (vi, ti, i) if x is not None)
                vaddr = exe_base + stub_start * 4
                jal = (0x03 << 26) | ((vaddr >> 2) & 0x3FFFFFF)
                pat = struct.pack("<I", jal)
                st = 0
                while True:
                    k = buf.find(pat, st)
                    if k < 0:
                        break
                    if k % 4 == 0:
                        s = patch_sig(ws, k // 4 + 2, fn)
                        if s:
                            s["ra_off"] = k + 8
                            sigs.append(s)
                    st = k + 1
            if sigs:
                rec["patch_sigs"] = sigs
    return calls, syscalls, unresolved


def scan_kernel_ram_refs(buf):
    """Absolute kernel-RAM loads/stores (inferred: code/data not separated)."""
    ws = words(buf)
    refs = {}
    lui = {}  # reg -> (index, hi)
    for i, w in enumerate(ws):
        op = w >> 26
        if op == 0x0F:
            rt = (w >> 16) & 31
            hi = w & 0xFFFF
            if hi in (0x8000, 0xA000, 0x0000):
                lui[rt] = (i, hi)
            else:
                lui.pop(rt, None)
            continue
        if 0x20 <= op <= 0x2E and op not in (0x27, 0x2C, 0x2D):
            rs = (w >> 21) & 31
            imm = w & 0xFFFF
            if imm & 0x8000:
                continue
            addr = None
            if rs == 0:
                addr = imm
            elif rs in lui and i - lui[rs][0] <= 4 and lui[rs][1] in (0x8000, 0xA000):
                addr = imm
            if addr is not None and addr < 0x10000:
                key = addr & ~3
                e = refs.setdefault(key, [0, 0])
                e[1 if op >= 0x28 else 0] += 1
    return refs


def classify(buf):
    if buf[:8] == b"PS-X EXE":
        return "psx-exe"
    if len(buf) < 64:
        return "data"
    jr_ra = buf.count(b"\x08\x00\xe0\x03")
    sp_adj = len(re.findall(rb"(?s)(?=[\x00-\xff][\xff]\xbd\x27)", buf))
    kb = max(1, len(buf) // 1024)
    if jr_ra / kb >= 0.5 and sp_adj / kb >= 0.3:
        return "code-like"
    if jr_ra >= 8 and sp_adj >= 8:
        return "contains-code"
    return "data"


def sha256(b):
    return hashlib.sha256(b).hexdigest()


def serial_from_boot(boot):
    m = re.search(r"([A-Z]{4})[_-]?(\d{3})\.?(\d{2})", boot.upper())
    return "%s-%s%s" % m.groups() if m else None


def region_of(serial, lic):
    if lic:
        l = lic.lower()
        if l.startswith("amer"):
            return "NTSC-U"
        if l.startswith("euro"):
            return "PAL"
        if l.startswith("inc"):
            return "NTSC-J"
    if serial:
        p = serial[:4]
        if p in ("SCUS", "SLUS", "PAPX"):
            return "NTSC-U"
        if p in ("SCES", "SLES", "SCED", "SLED"):
            return "PAL"
        if p in ("SCPS", "SLPS", "SLPM", "SCPM"):
            return "NTSC-J"
    return "unknown"


def scan_disc(path, hash_image=True):
    disc = Disc(path)
    files = disc.walk()
    byname = {n.upper().split(";")[0]: (n, l, s) for n, l, s in files}
    res = {"disc": os.path.basename(path), "image_file": os.path.basename(disc.image_path)}
    if hash_image:
        res["image_sha256"] = sha256(disc.image)
    cnf = byname.get("SYSTEM.CNF")
    boot = None
    if cnf:
        text, _, _ = disc.read_file(cnf[1], cnf[2])
        t = text.decode("latin-1")
        m = re.search(r"BOOT\s*=\s*cdrom:\\?\\?([^\s;]+)", t, re.I)
        boot = m.group(1).replace("\\", "/").upper() if m else None
        res["system_cnf"] = {k: v for k, v in re.findall(r"^\s*(\w+)\s*=\s*(\S+)", t, re.M)}
    res["boot_exe"] = boot
    res["serial"] = serial_from_boot(boot) if boot else None
    lic = disc.license_text()
    res["license_region_text"] = lic
    res["region"] = region_of(res["serial"], lic)
    out_files = []
    for name, lba, size in files:
        key = name.upper().split(";")[0]
        rec = {"file": name, "lba": lba, "size": size}
        if size == 0:
            rec["class"] = "empty"
            out_files.append(rec)
            continue
        try:
            buf, form2, n = disc.read_file(lba, size)
        except EOFError:
            rec["class"] = "outside data-track image (not scanned)"
            out_files.append(rec)
            continue
        if form2 * 2 > n:
            rec["class"] = "form2-stream (not scanned)"
            out_files.append(rec)
            continue
        cls = classify(buf)
        rec["class"] = cls
        exe_base = None
        if cls == "psx-exe":
            pc0, gp0, taddr, tsize = struct.unpack_from("<IIII", buf, 0x10)
            rec["exe"] = {"pc0": "0x%08x" % pc0, "t_addr": "0x%08x" % taddr, "t_size": tsize}
            exe_base = taddr - 0x800
            rec["sha256"] = sha256(buf)
        is_boot = boot is not None and key == boot.split(";")[0]
        rec["is_boot_exe"] = is_boot
        if is_boot and "sha256" not in rec:
            rec["sha256"] = sha256(buf)
        calls, sysc, unresolved = scan_calls(buf, exe_base)
        if cls == "data":
            # Raw data: a bare SYSCALL word or a vector load without a
            # function number is noise; keep only fully formed stubs.
            calls = [c for c in calls if c["fn"] is not None]
            for c in calls:
                c["in_data_file"] = True
            sysc = []
            unresolved = 0
        rec["calls"] = calls
        rec["syscalls"] = sysc
        rec["unresolved_fn"] = unresolved
        if cls in ("psx-exe", "code-like", "contains-code"):
            refs = scan_kernel_ram_refs(buf)
            rec["kernel_ram_refs"] = {"0x%04x" % k: v for k, v in sorted(refs.items())}
        out_files.append(rec)
    res["files"] = out_files
    return res


def summarize(res):
    fns = {}
    for f in res["files"]:
        for c in f.get("calls", []):
            k = "%s(%s)" % (c["table"], "??" if c["fn"] is None else "%02Xh" % c["fn"])
            fns.setdefault(k, set()).add(f["file"])
    return {k: sorted(v) for k, v in sorted(fns.items())}


if __name__ == "__main__":
    args = sys.argv[1:]
    out = None
    if "--json" in args:
        i = args.index("--json")
        out = args[i + 1]
        del args[i : i + 2]
    r = scan_disc(args[0])
    r["summary_functions"] = summarize(r)
    js = json.dumps(r, indent=1)
    if out:
        open(out, "w").write(js)
    else:
        print(js)
