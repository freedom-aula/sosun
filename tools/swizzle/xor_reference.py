#!/usr/bin/env python3
"""Reference addresses of the XOR texture modes, computed straight from addrlib's gfx10SwizzlePattern.h
(its tables as text, the equations evaluated bit by bit with no masks or lookup tables), for the golden
values of `victory-formats`' `texture::tests::xor_addresses_match_the_reference`.

Usage: xor_reference.py <gfx10SwizzlePattern.h>   (prints the Rust array)
"""
import random
import re
import sys

MODES = {
    # swizzle number: (block log2, thin table, thick table or None)
    17: (16, "64K_S_T", "64K_S3_T"),
    18: (16, "64K_D_T", None),
    21: (12, "4K_S_X", "4K_S3_X"),
    22: (12, "4K_D_X", None),
    24: (16, "64K_Z_X_1xaa", None),
    25: (16, "64K_S_X", "64K_S3_X"),
    26: (16, "64K_D_X", "64K_D3_X"),
    27: (16, "64K_R_X_1xaa", None),
}
# (packers log2, pipes log2) -> row of the tables, from their comments ("8 pipes (4 PKRs)" ...).
ROWS = {(0, 0): 0, (0, 1): 1, (1, 1): 1, (0, 2): 2, (1, 2): 2, (1, 3): 3, (2, 2): 4, (2, 3): 5, (2, 4): 6,
        (3, 3): 7, (3, 4): 8, (3, 5): 9, (4, 4): 10, (4, 5): 11, (4, 6): 12, (5, 5): 13, (5, 6): 14}


def section(text, name):
    m = re.search(re.escape(name) + r"\[[^\]]*\](?:\[[^\]]*\])?\s*=\s*\{(.*?)\n\};", text, re.S)
    return m.group(1)


class Header:
    def __init__(self, path):
        self.text = open(path).read()
        self.nib = {}
        for n in ("NIBBLE01", "NIBBLE2", "NIBBLE3", "NIBBLE4"):
            rows = []
            for line in section(self.text, "GFX10_SW_PATTERN_" + n).splitlines():
                m = re.match(r"\s*\{(.*?)\},?\s*//\s*(\d+)", line)
                if m:
                    rows.append([c.strip() for c in m.group(1).split(",") if c.strip()])
            self.nib[n] = rows
        self.info = {}

    def patinfo(self, table):
        if table not in self.info:
            rows = []
            for line in section(self.text, f"GFX10_SW_{table}_RBPLUS_PATINFO").splitlines():
                m = re.match(r"\s*\{\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+),\s*(\d+),\s*\}", line)
                if m:
                    rows.append([int(g) for g in m.groups()])
            self.info[table] = rows
        return self.info[table]

    def equations(self, table, packers, pipes, element_log2, block_log2):
        r = self.patinfo(table)[ROWS[(packers, pipes)] * 5 + element_log2]
        eq = []
        for n, i in zip(("NIBBLE01", "NIBBLE2", "NIBBLE3", "NIBBLE4"), r[1:]):
            eq += self.nib[n][i]
        return eq[:block_log2]


def evaluate(eq, x, y, z):
    """The block offset: bit i is the parity of the terms of equation i (S terms are 0: one sample)."""
    off = 0
    for i, e in enumerate(eq):
        v = 0
        if e != "0":
            for t in e.split("^"):
                c, n = t[0], int(t[1:])
                coord = {"X": x, "Y": y, "Z": z}.get(c)
                if coord is not None:
                    v ^= (coord >> n) & 1
        off |= v << i
    return off


def block_dims(block_log2, element_log2, thick):
    if thick:
        amp = (block_log2 - 10) // 3
        rest = (block_log2 - 10) % 3
        base = [[16, 8, 8], [8, 8, 8], [8, 8, 4], [8, 4, 4], [4, 4, 4]][element_log2]
        return [base[0] << amp, base[1] << (amp + rest // 2), base[2] << (amp + (rest != 0))]
    el = block_log2 - element_log2
    w = (el + 1) // 2
    return [1 << w, 1 << (el - w), 1]


def main():
    h = Header(sys.argv[1])
    rng = random.Random(7)
    out = []
    for mode, (log2, thin, thick) in MODES.items():
        for three_d in (False, True):
            if three_d and thick is None and mode not in (24, 27):
                continue
            table = thick if (three_d and thick) else thin
            is_thick = three_d and thick is not None
            for element_log2 in (0, 2, 4):
                for packers, pipes in ((2, 3), (4, 5), (0, 0)):
                    eq = h.equations(table, packers, pipes, element_log2, log2)
                    bw, bh, bd = block_dims(log2, element_log2, is_thick)
                    # A mip 0 of 3 x 2 blocks, 8 slices (or depth): addresses with the block index.
                    width, height = 3 * bw, 2 * bh
                    for _ in range(5):
                        x, y = rng.randrange(width), rng.randrange(height)
                        z = rng.randrange(8)
                        blocks = (y // bh) * 3 + x // bw
                        off = evaluate(eq, x, y, z)
                        slab, zin = (z // bd, z % bd) if is_thick else (z, 0)
                        out.append((mode, int(three_d), element_log2, packers, pipes, x, y, z,
                                    (blocks << log2) + off, slab))
    print("// (swizzle, 3D, element log2, packers log2, pipes log2, x, y, z, address in a slice, slice)")
    print("type Reference = (u32, bool, u32, u32, u32, u32, u32, u32, u64, u32);")
    print("const REFERENCE: &[Reference] = &[")
    for o in out:
        print("    (%d, %s, %d, %d, %d, %d, %d, %d, %#x, %d)," % (o[0], "true" if o[1] else "false", *o[2:]))
    print("];")


main()
