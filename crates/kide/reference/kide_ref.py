#!/usr/bin/env python3
"""Independent reference for the Kide format, written from crates/kide/SPEC.md (folder format 7) alone.

Covers §2 (core format), §3.1 (version records), §5.1-5.3 (manifest, version state,
names) and prints the §8 test vectors. `--check` runs self-checks.
"""
import hashlib
import random
import struct
import sys
import time


class Invalid(Exception):
    """§4.4: bytes break a rule of the spec."""


# §2.1 Conventions and parameters

def H(b):
    return hashlib.sha256(b).digest()


K = 10
S = 1 << K           # 1,024
MIN = S // 4         # 256
MAX = 8 * S          # 8,192
B = K - 5            # 5
W = 1 << B           # 32, the largest number of children of a node
M64 = (1 << 64) - 1
NO_CONTENT = bytes(32)  # §3.1 tombstone root
MAX_ENTRY = 4096 + 1 + 32 + 8 + 8  # §5.1: 4,145 bytes


def u64(n):
    return struct.pack("<Q", n)


def i64(n):
    return struct.pack("<q", n)


def rd_u64(b, off):
    return struct.unpack_from("<Q", b, off)[0]


# §2.2 Chunking

GEAR = [int.from_bytes(H(bytes([i]))[:8], "little") for i in range(256)]
NATURAL = 1 << (64 - K)  # g(i) < NATURAL  <=>  the top k bits of g(i) are zero


def cut_points(data):
    """§2.2, a literal port of the spec's function: end offset of every chunk."""
    cuts, start, h = [], 0, 0
    for i, byte in enumerate(data):
        h = ((h << 1) + GEAR[byte]) & M64
        size = i + 1 - start
        if size >= MIN and (h < NATURAL or size == MAX):
            cuts.append(i + 1)
            start = i + 1
    if start < len(data):
        cuts.append(len(data))
    return cuts


def manifest_cut_points(data, entry_ends):
    """§5.1, a literal port: a decided cut moves to the end of the entry holding byte i."""
    cuts, start, h, e = [], 0, 0, 0
    for i, byte in enumerate(data):
        h = ((h << 1) + GEAR[byte]) & M64
        if i < start:  # rest of an entry already cut after
            continue
        size = i + 1 - start
        if size >= MIN and (h < NATURAL or size == MAX):
            while entry_ends[e] <= i:
                e += 1
            cuts.append(entry_ends[e])
            start = entry_ends[e]
    if start < len(data):
        cuts.append(len(data))
    return cuts


class Chunker:
    """§2.2 streaming form: any split of the input yields the same chunks."""

    def __init__(self):
        self.h, self.size, self.pending = 0, 0, bytearray()

    def feed(self, piece):
        out, h, size, last = [], self.h, self.size, 0
        for i, byte in enumerate(piece):
            h = ((h << 1) + GEAR[byte]) & M64
            size += 1
            if size >= MIN and (h < NATURAL or size == MAX):
                self.pending += piece[last:i + 1]
                out.append(bytes(self.pending))
                self.pending.clear()
                last, size = i + 1, 0
        self.pending += piece[last:]
        self.h, self.size = h, size
        return out

    def finish(self):
        return [bytes(self.pending)] if self.pending else []


def split_at(data, cuts):
    starts = [0] + cuts[:-1]
    return [data[a:b] for a, b in zip(starts, cuts)]


def validate_chunk(chunk, manifest=False):
    """§2.1: a chunk is at most `max` bytes; a manifest chunk at most `max` + 4,144 (§5.1)."""
    if len(chunk) > MAX + (MAX_ENTRY - 1 if manifest else 0):
        raise Invalid("chunk too long")


# §2.3 Nodes. An entry is (id, height, bytes).

def height(id_):
    """§2.3: leading zero bits of the id (big-endian) divided by b, rounded down."""
    return (256 - int.from_bytes(id_, "big").bit_length()) // B


def chunk_entry(chunk, store):
    cid = H(chunk)
    store[cid] = chunk
    return (cid, height(cid), len(chunk))


def make_node(level, children, store):
    """§2.3 encoding: level u8 | (id | bytes u64) per child.
    Its height is the last child's, hence the height of the last chunk below it (0 if none)."""
    enc = bytes([level]) + b"".join(c[0] + u64(c[2]) for c in children)
    nid = H(enc)
    store[nid] = enc
    return (nid, children[-1][1] if children else 0, sum(c[2] for c in children))


def build(chunks, store):
    """§2.3 build: a group closes after an entry of height >= level, or its W-th entry."""
    if not chunks:
        return make_node(1, [], store)
    items, level = chunks, 1
    while True:
        nxt, group = [], []
        for e in items:
            closes = e[1] >= level or len(group) + 1 == W
            group.append(e)
            if closes:
                nxt.append(make_node(level, group, store))
                group = []
        if group:
            nxt.append(make_node(level, group, store))
        items = nxt
        if len(items) == 1:
            return items[0]
        level += 1


def content_root(data, store, entry_ends=None):
    cuts = cut_points(data) if entry_ends is None else manifest_cut_points(data, entry_ends)
    return build([chunk_entry(c, store) for c in split_at(data, cuts)], store)


def node_children(enc):
    return [(enc[1 + 40 * j:33 + 40 * j], rd_u64(enc, 33 + 40 * j)) for j in range((len(enc) - 1) // 40)]


def content_length(id_, level, store):
    """Length of the content below a child: a chunk's length, or the sum of a node's `bytes`."""
    return len(store[id_]) if level == 0 else sum(b for _, b in node_children(store[id_]))


def validate_node(enc, store):
    """§2.3 validation; children are checked only when available in `store`."""
    if len(enc) < 1 or (len(enc) - 1) % 40:
        raise Invalid("node length")
    level, kids = enc[0], node_children(enc)
    if level < 1:
        raise Invalid("node level 0")
    if not (1 <= len(kids) <= W or (level == 1 and not kids)):
        raise Invalid("node child count %d" % len(kids))
    if any(b < 1 for _, b in kids) or sum(b for _, b in kids) > M64:
        raise Invalid("child bytes")
    for cid, nbytes in kids:
        if cid in store:
            if level > 1 and store[cid][0] != level - 1:
                raise Invalid("child level")
            if content_length(cid, level - 1, store) != nbytes:
                raise Invalid("child bytes mismatch")


def walk(root_id, store):
    """Every node below a root (inclusive), as (id, encoding); chunks are not yielded."""
    todo = [root_id]
    while todo:
        nid = todo.pop()
        enc = store[nid]
        yield nid, enc
        if enc[0] > 1:
            todo.extend(cid for cid, _ in node_children(enc))


def leaf_chunks(root_id, store):
    """§2.3 lookup order: the chunks of a tree, left to right."""
    enc = store[root_id]
    if enc[0] == 1:
        return [store[cid] for cid, _ in node_children(enc)]
    return [c for cid, _ in node_children(enc) for c in leaf_chunks(cid, store)]


# §3.1 Version records

def encode_record(parents, root, state=b"", change=b""):
    return u64(len(parents)) + b"".join(parents) + root + u64(len(change)) + change + state


def decode_record(enc, store=None):
    """§3.1 decode and validate; returns (parents, root, state).
    With `store`, available parents are checked too (no merge record under a merge record)."""
    if len(enc) < 8:
        raise Invalid("record too short")
    n = rd_u64(enc, 0)
    if n > W:
        raise Invalid("more than W parents")
    if len(enc) < 8 + 32 * n + 40:
        raise Invalid("record truncated")
    parents = [enc[8 + 32 * j:40 + 32 * j] for j in range(n)]
    root = enc[8 + 32 * n:40 + 32 * n]
    change_len = rd_u64(enc, 40 + 32 * n)
    start = 48 + 32 * n
    if change_len > MAX + 8 or start + change_len > len(enc):
        raise Invalid("invalid change metadata length")
    state = enc[start + change_len:]
    if n >= 2 and change_len:
        raise Invalid("merge record with operation metadata")
    if any(a >= b for a, b in zip(parents, parents[1:])):
        raise Invalid("parents unsorted or duplicate")
    if len(state) > S:
        raise Invalid("state longer than S")
    if root == NO_CONTENT and (state or not parents):
        raise Invalid("tombstone with state or without parent")
    if store is not None and n >= 2:
        for p in parents:
            if p in store and rd_u64(store[p], 0) >= 2:
                raise Invalid("merge record under a merge record")
    return parents, root, state


# §5.2 Version state of the folder profile
REGULAR, EXECUTABLE, SYMLINK, DIRECTORY = b"\x00", b"\x01", b"\x02", b"\x03"


def record_change(enc):
    n = rd_u64(enc, 0)
    at = 48 + 32 * n
    return enc[at:at + rd_u64(enc, at - 8)]


def validate_change(change):
    if not change:
        return
    if change[0] == 2 and len(change) == 33:
        return
    if change[0] != 1:
        raise Invalid("unknown folder change")
    paths, offset = [], 1
    for _ in range(2):
        if offset + 2 > len(change):
            raise Invalid("short move path length")
        size = int.from_bytes(change[offset:offset + 2], "little")
        offset += 2
        p = change[offset:offset + size]
        if len(p) != size or not valid_path(p):
            raise Invalid("invalid move path")
        paths.append(p)
        offset += size
    if offset != len(change) or paths[0] == paths[1] or paths[0].endswith(b"/") != paths[1].endswith(b"/"):
        raise Invalid("invalid move operands or trailing bytes")


def validate_folder_record(enc, store=None):
    parents, root, state = decode_record(enc, store)
    validate_change(record_change(enc))
    if record_change(enc) and len(parents) != 1:
        raise Invalid("folder operation needs exactly one parent")
    if root != NO_CONTENT and (len(state) != 1 or state[0] > 3):
        raise Invalid("folder state is one byte 0..3")
    return parents, root, state


# §5.3 Names (what decoders check; NFC is deliberately not checked)

def valid_component(c, top):
    return (1 <= len(c) <= 255 and b"/" not in c and b"\0" not in c
            and c not in (b".", b"..") and not c.lower().endswith(b".kide-tmp")
            and not (top and c.lower() == b".kide"))  # bytes.lower() folds ASCII only


def valid_path(p):
    """§5.1/§5.3: components joined by `/`, plus one trailing `/` for an empty directory;
    at most 4,096 bytes including that `/`; valid UTF-8."""
    if not 1 <= len(p) <= 4096:
        return False
    try:
        p.decode("utf-8")  # strict: rejects overlong forms and surrogates
    except UnicodeDecodeError:
        return False
    comps = (p[:-1] if p.endswith(b"/") else p).split(b"/")
    return all(valid_component(c, j == 0) for j, c in enumerate(comps))


# §5.1 Manifest

ENTRY_TAIL = 32 + 8 + 8


def encode_entry(path, version, size, mtime):
    return path + b"\0" + version + u64(size) + i64(mtime)


def manifest_bytes(entries):
    """Entries (path, version, size, mtime) sorted by path; returns the bytes and entry ends."""
    entries = sorted(entries)
    if any(a[0] == b[0] for a, b in zip(entries, entries[1:])):
        raise Invalid("duplicate path")
    data, ends = bytearray(), []
    for e in entries:
        data += encode_entry(*e)
        ends.append(len(data))
    return bytes(data), ends


def manifest_root(entries, store):
    data, ends = manifest_bytes(entries)
    return content_root(data, store, ends)


def decode_entries(chunk):
    """Decode one manifest chunk on its own; it must hold whole entries only."""
    out, off = [], 0
    while off < len(chunk):
        z = chunk.find(b"\0", off)
        if z < 0 or z + 1 + ENTRY_TAIL > len(chunk):
            raise Invalid("partial entry")
        v = z + 1
        out.append((chunk[off:z], chunk[v:v + 32], rd_u64(chunk, v + 32),
                    struct.unpack_from("<q", chunk, v + 40)[0]))
        off = v + ENTRY_TAIL
    return out


def validate_manifest_chunk(chunk):
    """§5.1 validation of one chunk: complete entries, valid and strictly ascending paths."""
    validate_chunk(chunk, manifest=True)
    entries = decode_entries(chunk)
    if not all(valid_path(e[0]) for e in entries):
        raise Invalid("invalid path")
    if any(a[0] >= b[0] for a, b in zip(entries, entries[1:])):
        raise Invalid("paths not strictly ascending")
    return entries


def validate_manifest(root_id, store):
    """§5.1 whole-manifest validation; needs every record and content root in `store`."""
    entries = [e for c in leaf_chunks(root_id, store) for e in validate_manifest_chunk(c)]
    if any(a[0] >= b[0] for a, b in zip(entries, entries[1:])):
        raise Invalid("paths not strictly ascending across chunks")
    live_files, live_dirs = set(), set()
    for path, version, size, mtime in entries:
        _, root, state = validate_folder_record(store[version], store)
        is_dir = state == DIRECTORY
        if (root == NO_CONTENT or is_dir) and (size or mtime):
            raise Invalid("tombstone or directory with size or mtime")
        if state == SYMLINK and mtime:
            raise Invalid("symbolic link with mtime")
        if root == NO_CONTENT:
            continue  # the trailing-`/` rule is applied to live entries only (see report)
        if path.endswith(b"/") != is_dir:
            raise Invalid("trailing / does not match the directory kind")
        if size != content_length(root, 1, store):
            raise Invalid("size differs from content length")
        (live_dirs if is_dir else live_files).add(path)
    for path in live_files | live_dirs:
        for j, byte in enumerate(path):
            if byte == 0x2F and (path[:j] in live_files or
                                 (path[:j + 1] in live_dirs and j + 1 < len(path))):
                raise Invalid("live entry below a live file or empty directory")
    return entries


# §8 Inputs

def input_T():
    return "".join("%05d %s\n" % (i, "lorem ipsum " * (i % 7)) for i in range(3000)).encode()


def input_B():
    return b"".join(H(struct.pack("<I", i)) for i in range(8192))


def input_L():
    return "".join('{"id":%d,"tag":"%s"},' % (i, "ab" * (i % 5)) for i in range(6000)).encode()


def inputs():
    return {"T": input_T(), "B": input_B(), "L": input_L(), "Z": bytes(1 << 20)}


def records(store):
    roots = {name: content_root(data, store)[0] for name, data in
             [("hello", b"hello\n"), ("hello world", b"hello world\n"), ("empty", b""), ("B", input_B())]}
    v = {}
    v["v1"] = encode_record([], roots["hello"], REGULAR)
    v["v2"] = encode_record([H(v["v1"])], roots["hello world"], REGULAR)
    v["v3"] = encode_record([H(v["v2"])], NO_CONTENT)
    v["vx"] = encode_record([H(v["v1"])], roots["hello"], EXECUTABLE)
    v["vb"] = encode_record([], roots["B"], REGULAR)
    v["vd"] = encode_record([], roots["empty"], DIRECTORY)
    for enc in v.values():
        store[H(enc)] = enc
    return v


def manifest_entries(v):
    return [(b"a.txt", H(v["v1"]), 6, 1761134400000000000),
            (b"b/c.bin", H(v["vb"]), 262144, 1761138000123456789),
            (b"d/", H(v["vd"]), 0, 0)]


def vectors():
    store, rows = {}, []
    data = inputs()
    for i in (0, 1, 255):
        rows.append(("`GEAR[%d]`" % i, "`0x%016x`" % GEAR[i]))
    trees = {}
    for name, d in data.items():
        cuts = cut_points(d)
        trees[name] = (cuts, build([chunk_entry(c, store) for c in split_at(d, cuts)], store)[0])
    for name in "TBL":
        cuts = trees[name][0]
        rows.append(("%s: chunk count; first three cut offsets" % name,
                     "%d; %s" % (len(cuts), ", ".join(map(str, cuts[:3])))))
    cuts, root = trees["Z"]
    widest = max((len(enc) - 1) // 40 for _, enc in walk(root, store))
    rows.append(("Z: chunk count; widest node", "%d; %d children" % (len(cuts), widest)))
    small = {"empty content": b"", "`hello\\n`": b"hello\n", "`hello world\\n`": b"hello world\n"}
    named = [(n, trees[n][1]) for n in "TBLZ"] + [(n, content_root(d, store)[0]) for n, d in small.items()]
    for name, rid in named:
        rows.append(("Root of %s" % name, "`%s`, level %d" % (rid.hex(), store[rid][0])))
    v = records(store)
    for name, enc in v.items():
        rows.append((name, "`%s`" % H(enc).hex()))
    mroot = manifest_root(manifest_entries(v), store)[0]
    validate_manifest(mroot, store)
    rows.append(("Manifest root", "`%s`, level %d" % (mroot.hex(), store[mroot][0])))
    moved = b"\x01\x05\x00a.txt\x0b\x00renamed.txt"
    vm = encode_record([H(v["v1"])], decode_record(v["v1"])[1], REGULAR, moved)
    vr = encode_record([H(vm)], decode_record(v["v1"])[1], REGULAR, b"\x02" + mroot)
    for name, enc in [("vm (move)", vm), ("vr (forward restore)", vr)]:
        validate_folder_record(enc)
        rows.append((name, "`%s`" % H(enc).hex()))
    return rows


# --check

def check(name, ok, detail=""):
    print("%s %s%s" % ("ok  " if ok else "FAIL", name, (": " + detail) if detail else ""))
    return ok


def rejects(fn, *args):
    try:
        fn(*args)
    except Invalid:
        return True
    return False


def random_split(data, rng, max_piece):
    pieces, off = [], 0
    while off < len(data):
        n = rng.randint(0, max_piece)
        pieces.append(data[off:off + n])
        off += n
    return pieces


def stream_chunks(pieces):
    c, out = Chunker(), []
    for p in pieces:
        out += c.feed(p)
    return out + c.finish()


def moved_cuts(data, ends):
    """Independent formulation of the §5.1 rule (the first reading): run §2.2 and move each
    decided cut to the next entry end, resetting `start` there."""
    cuts, start, h, ends = [], 0, 0, sorted(ends)
    k = 0
    for i, byte in enumerate(data):
        h = ((h << 1) + GEAR[byte]) & M64
        if i + 1 - start >= MIN and (h < NATURAL or i + 1 - start == MAX):
            while ends[k] < i + 1:
                k += 1
            start = ends[k]
            cuts.append(start)
    return cuts + ([len(data)] if start < len(data) else [])


def check_chunking(data, rng):
    results = []
    for name in ("T", "B", "L", "Z"):
        d = data[name]
        want = split_at(d, cut_points(d))
        sizes = (1, 7, 300, 10000) if name != "Z" else (5000, 70000)
        same = all(stream_chunks(random_split(d, rng, mp)) == want for mp in sizes)
        results.append(check("streaming equality " + name, same))
    t, store = data["T"], {}
    root = content_root(t, store)[0]
    pos = len(t) // 2
    edited = t[:pos] + b"X" + t[pos:]
    reverted = edited[:pos] + edited[pos + 1:]
    streamed = build([chunk_entry(c, store) for c in stream_chunks(random_split(t, rng, 999))], store)[0]
    results.append(check("history independence T", content_root(reverted, {})[0] == root == streamed))
    old_chunks = set(split_at(t, cut_points(t)))
    old_nodes = {nid for nid, _ in walk(root, store)}
    worst_c = worst_n = 0
    for _ in range(25):
        p = rng.randrange(len(t) + 1)
        e = t[:p] + bytes([rng.randrange(256)]) + t[p:]
        s2 = {}
        r2 = content_root(e, s2)[0]
        worst_c = max(worst_c, len(set(split_at(e, cut_points(e))) - old_chunks))
        worst_n = max(worst_n, len({nid for nid, _ in walk(r2, s2)} - old_nodes))
    results.append(check("locality T (25 random inserts)", worst_c <= 3,
                         "at most %d new chunks, %d new nodes" % (worst_c, worst_n)))
    return results


def check_trees(data):
    results = []
    for name, d in data.items():
        st = {}
        r = content_root(d, st)[0]
        widest, err = 0, None
        try:
            for _, enc in walk(r, st):
                validate_node(enc, st)
                widest = max(widest, (len(enc) - 1) // 40)
            for c in leaf_chunks(r, st):
                validate_chunk(c)
        except Invalid as x:
            err = str(x)
        same = b"".join(leaf_chunks(r, st)) == d
        results.append(check("nodes valid, widest <= W, content round-trips: " + name,
                             err is None and widest <= W and same, err or "widest %d" % widest))
    st = {}
    flat = [(H(u64(i)), 0, 1) for i in range(W * W + 3)]  # adversarial: all heights 0
    r = build(flat, st)[0]
    widest = max((len(enc) - 1) // 40 for _, enc in walk(r, st))
    results.append(check("W bound with %d height-0 entries" % len(flat), widest <= W,
                         "root level %d, widest %d" % (st[r][0], widest)))
    bad = [b"", b"\x00", b"\x01" + H(b"x") + u64(0), b"\x02", b"\x01" + bytes(39),
           b"\x01" + (H(b"x") + u64(1)) * (W + 1), b"\x01" + (H(b"x") + u64(M64)) * 2]
    results.append(check("invalid nodes rejected", all(rejects(validate_node, b, {}) for b in bad)))
    return results


def check_manifest(rng):
    results, st = [], {}
    entries, live = [], {}
    for i in range(4000):
        depth = rng.randint(1, 4)
        path = "/".join("d%x" % rng.randrange(40) for _ in range(depth - 1)) + ("/" if depth > 1 else "")
        path = (path + "f%05d.txt" % i).encode()
        content = b"%d\n" % i
        rec = encode_record([], content_root(content, st)[0], REGULAR)
        if i % 10 == 0:  # some deleted files
            rec = encode_record([H(rec)], NO_CONTENT)
        st[H(rec)] = rec
        size = 0 if i % 10 == 0 else len(content)
        entries.append((path, H(rec), size, 0 if i % 10 == 0 else i * 1000))
    long_path = ("x" * 255 + "/") * 15 + "y" * 255 + "/"  # 4,096 bytes with the trailing /
    vd = encode_record([], content_root(b"", st)[0], DIRECTORY)
    st[H(vd)] = vd
    entries += [(long_path.encode(), H(vd), 0, 0), ("é/".encode(), H(vd), 0, 0)]
    data_m, ends = manifest_bytes(entries)
    mr = content_root(data_m, st, ends)[0]
    chunks = leaf_chunks(mr, st)
    ends_set, off, ok, max_excess = set(ends), 0, True, 0
    for c in chunks:
        ok &= off == 0 or off in ends_set
        dec = validate_manifest_chunk(c)
        max_excess = max(max_excess, len(c) - len(encode_entry(*dec[-1])))
        off += len(c)
        ok &= off in ends_set
    ok &= b"".join(chunks) == data_m and max_excess <= MAX - 1
    ok &= len(validate_manifest(mr, st)) == len(entries)
    shuffled = entries[:]
    rng.shuffle(shuffled)
    ok &= manifest_root(shuffled, {})[0] == mr
    results.append(check("manifest chunks at entry boundaries, decode alone, manifest valid", ok,
                         "%d entries, %d chunks, max bytes before last entry %d" %
                         (len(entries), len(chunks), max_excess)))
    z, step = bytes(1 << 16), 3000
    zends = list(range(step, len(z), step)) + [len(z)]
    want = [9000 * j for j in range(1, 8)] + [1 << 16]
    same = all(manifest_cut_points(d, e) == moved_cuts(d, e) for d, e in [(z, zends), (data_m, ends)])
    results.append(check("manifest_cut_points: forced cuts move to entry ends, equals first reading",
                         manifest_cut_points(z, zends) == want and same))

    bad = [b"", b"/a", b"a//b", b"a/./b", b"a/../b", b".kide/x", b".KiDe/", b"x.kide-tmp", b"x.KIDE-TMP",
           b"a/.b.kide-tmp/c", b"\xff", b"a" * 256, b"a\x00b", b"a/" * 2048 + b"b"]
    good = [b"a/.kide", "é".encode(), b".kide-tmp.x", b"d/", ("İKIDE").encode()]
    results.append(check("paths: reserved and invalid rejected, non-NFC accepted",
                         not any(map(valid_path, bad)) and all(map(valid_path, good))))

    # Whole-manifest rules, each broken once on top of the valid §8 manifest.
    v = records(st)
    a, vb, vd = H(v["v1"]), H(v["vb"]), H(v["vd"])
    base = manifest_entries(v)
    broken = [
        [(b"a.txt", a, 7, 1)] + base[1:],                          # size differs
        base + [(b"e", H(v["v3"]), 0, 5)],                         # tombstone with mtime
        base + [(b"e", vd, 0, 0)],                                 # directory without /
        base + [(b"e/", a, 6, 1)],                                 # file with /
        base + [(b"a.txt/x", a, 6, 1)],                            # below a live file
        base + [(b"d/x", a, 6, 1)],                                # below a live empty directory
        base + [(b"a.txt/", vd, 0, 0)],                            # empty directory below a file
    ]
    rej = []
    for es in broken:
        s2 = dict(st)
        rej.append(rejects(validate_manifest, manifest_root(es, s2)[0], s2))
    c1, c2 = (encode_entry(*e) for e in (base[1], base[0]))    # valid chunks, wrong order
    s2 = dict(st)
    r = build([chunk_entry(c1, s2), chunk_entry(c2, s2)], s2)[0]
    rej.append(rejects(validate_manifest, r, s2))
    s2 = dict(st)
    link = encode_record([], content_root(b"target", s2)[0], SYMLINK)
    s2[H(link)] = link
    s3 = dict(s2)
    rej.append(rejects(validate_manifest, manifest_root(base + [(b"l", H(link), 6, 1)], s3)[0], s3))  # link with mtime
    s3 = dict(s2)
    ok_link = not rejects(validate_manifest, manifest_root(base + [(b"l", H(link), 6, 0)], s3)[0], s3)
    s2 = dict(st)
    ok_dead = ok_link and not rejects(validate_manifest, manifest_root(base + [(b"g/", H(v["v3"]), 0, 0)], s2)[0], s2)
    results.append(check("whole-manifest rules reject each violation",
                         all(rej) and ok_dead, "%d of %d rejected" % (sum(rej), len(rej))))
    return results


def check_records():
    st = {}
    v = records(st)
    rt = all(encode_record(*decode_record(enc, st), record_change(enc)) == enc for enc in v.values())
    rt &= all(validate_folder_record(enc, st) for enc in v.values())
    a, b2 = sorted([H(b"a"), H(b"b")])
    root = content_root(b"x", st)[0]
    merge = encode_record([a, b2], root, b"\0")
    st[H(merge)] = merge
    c = H(v["v1"])
    under = encode_record(sorted([H(merge), c]), root, b"\0")
    bad = [encode_record([b2, a], root, b"\0"),
           encode_record([a, a], root, b"\0"),
           encode_record([a], NO_CONTENT, b"\0"),
           encode_record([], NO_CONTENT),
           encode_record([H(u64(i)) for i in range(W + 1)], root, b"\0"),
           encode_record([], root, bytes(S + 1)),
           u64(2) + a + root]
    rej = all(rejects(decode_record, b, st) for b in bad) and rejects(decode_record, under, st)
    rej &= not rejects(decode_record, merge, st) and rejects(validate_folder_record, encode_record([], root, b"\4"))
    moved = b"\x01\x05\x00a.txt\x05\x00b.txt"
    op = encode_record([c], root, REGULAR, moved)
    rt &= encode_record(*decode_record(op), record_change(op)) == op
    rt &= not rejects(validate_folder_record, op)
    rej &= rejects(validate_folder_record, encode_record([c], root, REGULAR, b"\x03"))
    rej &= rejects(decode_record, encode_record([c], root, REGULAR, bytes(MAX + 9)))
    return [check("version records round-trip", rt),
            check("version records: unsorted, duplicate, tombstone state or no parent, "
                  "merge under merge rejected", rej)]


def self_checks():
    rng, data = random.Random(1), inputs()
    results = check_chunking(data, rng) + check_trees(data) + check_manifest(rng) + check_records()
    return all(results)


def main():
    t0 = time.time()
    if sys.argv[1:] == ["--check"]:
        ok = self_checks()
        print("%.1f s" % (time.time() - t0))
        sys.exit(0 if ok else 1)
    for value, expected in vectors():
        print("| %s | %s |" % (value, expected))


if __name__ == "__main__":
    main()
