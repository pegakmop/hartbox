#!/usr/bin/env python3
"""Turn the templates already in `src/spec.rs` back into a SPECRUN profile.

`gen_spec.py` rebuilds `spec.rs` from scratch out of profiles, so a new profile
(say, the LZMA loops of `7z`) would drop every template mined earlier. Feeding
this script's output to `gen_spec.py` as one more colon-separated profile keeps
them. The counts are not recoverable, but the functions are numbered by rank
(`spec_0` is the hottest), so the count is rebuilt as a Zipf-like weight of the
rank. Each profile is normalised to the same total by `gen_spec.py` anyway.

Usage: spec_to_runs.py <src/spec.rs> > old_runs.txt
"""
import re
import sys

WORD0 = re.compile(r"\(Op::(\w+) as u64\) \| (\d+) << 8 \| (\d+) << 16 \| (\d+) << 24")
WORD1 = re.compile(r"0x([0-9a-f]{8})u64 \| (\d+) << 32")
ENTRY = re.compile(r"Spec \{ words: &\[(.*)\], step: spec_(\d+) \},")
STATIC = re.compile(r"static W_(\d+): \[u64; \d+\] = \[(.*)\];")


def op_names():
    """Opcode names by ordinal, from the enum in src/insn.rs."""
    sys.path.insert(0, __file__.rsplit("/", 1)[0])
    import gen_spec
    return {v: k for k, v in gen_spec.op_ordinals().items()}


def main():
    path = sys.argv[1]
    out = []
    names = op_names()
    for line in open(path):
        m = STATIC.search(line)
        if m:
            # Current format: plain hex words in one static per template.
            rank = int(m.group(1))
            words = [int(w.strip(), 16) for w in m.group(2).split(",")]
            toks = []
            for w0, w1 in zip(words[0::2], words[1::2]):
                v = w1 & 0xFFFF_FFFF
                if v >= 1 << 31:
                    v -= 1 << 32
                toks.append(f"{names[w0 & 0xFF]}:{(w0 >> 8) & 0xFF}:{(w0 >> 16) & 0xFF}:{(w0 >> 24) & 0xFF}:{v}:{(w1 >> 32) & 0xFF}")
            count = int(1e6 / ((rank + 1) ** 0.8 * len(toks))) + 1
            out.append(f"SPECRUN {count} {len(toks)} " + " ".join(toks))
            continue
        m = ENTRY.search(line)
        if not m:
            continue
        body, rank = m.group(1), int(m.group(2))
        w0 = WORD0.findall(body)
        w1 = WORD1.findall(body)
        assert len(w0) == len(w1) and w0, line[:80]
        toks = []
        for (op, rd, rs1, rs2), (imm, ln) in zip(w0, w1):
            v = int(imm, 16)
            if v >= 1 << 31:
                v -= 1 << 32
            toks.append(f"{op}:{rd}:{rs1}:{rs2}:{v}:{ln}")
        count = int(1e6 / ((rank + 1) ** 0.8 * len(toks))) + 1
        out.append(f"SPECRUN {count} {len(toks)} " + " ".join(toks))
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()
