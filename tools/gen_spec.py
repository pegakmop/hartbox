#!/usr/bin/env python3
"""Generate `src/spec.rs` from the sampled runs a `--features counters` build prints.

A run is a sequence of instructions that share their exact operand tuple, so the
generated function can name every register and immediate at compile time: no
operand index loads, no register-file round trip between dependent
instructions, and one dispatch for the whole sequence.

Usage: gen_spec.py <runs.txt> <src/spec.rs> [max templates]
"""
import sys

ALU_RR = {
    "Add", "Sub", "And", "Or", "Xor", "Sll", "Srl", "Sra", "Slt", "Sltu", "Mul",
    "Addw", "Subw", "Sllw", "Srlw", "Sraw", "Mulw",
}
ALU_IMM = {
    "Addi", "Andi", "Ori", "Xori", "Slli", "Srli", "Srai", "Addiw", "Slti",
    "Sltiu", "Slliw", "Srliw", "Sraiw",
}
PURE = {"Lui", "Auipc"}
LOADS = {
    "Ld": (8, True), "Lw": (4, True), "Lwu": (4, False), "Lh": (2, True),
    "Lhu": (2, False), "Lb": (1, True), "Lbu": (1, False),
}
STORES = {"Sd": 8, "Sw": 4, "Sh": 2, "Sb": 1}
BRANCHES = {"Beq", "Bne", "Blt", "Bge", "Bltu", "Bgeu"}
JUMPS = {"Jal", "Jalr"}
OPS = {}


def op_ordinals():
    """Opcode numbers, following the enum's declaration order."""
    ops = {}
    inside = False
    n = 0
    for line in open("src/insn.rs"):
        line = line.split("//")[0].strip()
        if line.startswith("pub enum Op {"):
            inside = True
            continue
        if inside and line.startswith("}"):
            break
        if not inside or not line or line.startswith("#") or line.startswith("pub enum"):
            continue
        line = line.rstrip(",")
        if "=" in line:
            name, val = line.split("=", 1)
            name, val = name.strip(), val.strip()
            n = int(val, 0)
            ops[name] = n
        else:
            ops[line] = n
        n += 1
    return ops


def parse(path):
    runs = []
    for line in open(path, errors="replace"):
        parts = line.split()
        if "SPECRUN" not in parts:
            continue
        i = parts.index("SPECRUN")
        count = int(parts[i + 1])
        k = int(parts[i + 2])
        insns = []
        for tok in parts[i + 3:i + 3 + k]:
            op, rd, rs1, rs2, imm, ln = tok.split(":")
            insns.append({
                "op": op, "rd": int(rd), "rs1": int(rs1), "rs2": int(rs2),
                "imm": int(imm), "len": int(ln),
            })
        runs.append((count, insns))
    return runs


MEM = set(LOADS) | set(STORES) | BRANCHES | JUMPS


def usable(insns):
    if len(insns) < 3 or len(insns) > 64:
        return False
    # A long run is worth it only when it is straight ALU code (a calibration
    # loop, an unrolled hash round): with memory accesses it bloats the code.
    if len(insns) > 48 and any(x["op"] in MEM for x in insns):
        return False
    # Runs that carry a branch are the ones that close a loop body: keep them
    # short, so they stay in the instruction cache and rarely bail.
    if any(x["op"] in BRANCHES for x in insns):
        if len(insns) > 20 or sum(1 for x in insns if x["op"] in BRANCHES) > 2:
            return False
    for k, x in enumerate(insns):
        if x["op"] not in ALU_RR | ALU_IMM | PURE | MEM:
            return False
        # Only the last instruction may transfer control.
        if x["op"] in JUMPS and k + 1 != len(insns):
            return False
        if x["len"] not in (2, 4):
            return False
        if x["rd"] > 31 or x["rs1"] > 31 or x["rs2"] > 31:
            return False
        # A pure write to x0 is dead, a load to x0 is not.
        if x["rd"] == 0 and (x["op"] in LOADS):
            return False
    return True


def sig(insns):
    return tuple((x["op"], x["rd"], x["rs1"], x["rs2"], x["imm"], x["len"]) for x in insns)


CHAIN_PARAM = 'chain: Chain, '
CHAIN_ARG = 'chain, '

def emit_fn(t, insns, out):
    """One specialised superinstruction for `insns`."""
    reads_after = []
    for k in range(len(insns)):
        later = set()
        for x in insns[k + 1:]:
            later.add(x["rs1"])
            if x["op"] not in ALU_IMM and x["op"] not in PURE:
                later.add(x["rs2"])
        reads_after.append(later)

    out.append("#[inline(never)]")
    out.append(
        f"unsafe fn spec_{t}({CHAIN_PARAM}tlb: *mut crate::hart::FastTlb, h: &mut Hart, bus: &mut Bus, d: *const DInsn, "
        f"fa: u64, fb: u64) -> u64 {{"
    )
    live = {}
    skip_next = False
    mem = []          # (base token, imm, width) -> value expression, same run only
    nlocals = 0
    pc_off = 0
    ended = False
    total = sum(x["len"] for x in insns)
    k = 0
    while k < len(insns):
        x = insns[k]
        op, rd, rs1, rs2 = x["op"], x["rd"], x["rs1"], x["rs2"]
        at = ""

        def flush(indent):
            # Every value is stored as soon as it is computed unless it is dead,
            # so a fallback finds the register file up to date.
            pass

        def operand(r):
            v = live.get(r)
            return v if v is not None else f"h.rx_c::<{r}>()"

        def written_later(r):
            return any(y["rd"] == r for y in insns[k + 1:])

        def base_token(r):
            """Identity of the address base: a local stays SSA, a register can change."""
            return ("reg", r) if r not in live else ("val", live[r])

        def drop_base(token):
            mem[:] = [e for e in mem if e[0] != token]

        def find_mem(token, imm):
            for e in mem:
                if e[0] == token and e[1] == imm:
                    return e
            return None

        def narrowed(expr, w, signed):
            if w == 8:
                return expr
            if w == 4:
                return f"(({expr} as u32) as i32 as i64 as u64)" if signed else f"(({expr} as u32) as u64)"
            if w == 2:
                return f"((({expr} as u16) as i16) as i64 as u64)" if signed else f"(({expr} as u16) as u64)"
            return f"((({expr} as u8) as i8) as i64 as u64)" if signed else f"(({expr} as u8) as u64)"

        if op in BRANCHES:
            a = operand(rs1)
            b = "0" if (x["op"] in ALU_IMM or x["op"] in BRANCHES) and rs2 == 0 else operand(rs2)
            target = (pc_off + x["imm"]) & 0xFFFF_FFFF_FFFF_FFFF
            out.append(f"    if spec_taken(Op::{op}, {a}, {b}) {{")
            out.append(f"        h.pc = pc_of(chain, h, d).wrapping_add({target}u64);")
            out.append(f"        return finish_direct({CHAIN_ARG}tlb, h, bus, d.add({k}), (*d.add({k})).index as u64 + 1);")
            out.append("    }")
            pc_off += x["len"]
            k += 1
            continue
        if op == "Jal":
            ret = (pc_off + x["len"]) & 0xFFFF_FFFF_FFFF_FFFF
            target = (pc_off + x["imm"]) & 0xFFFF_FFFF_FFFF_FFFF
            if rd != 0:
                out.append(f"    h.set_x_c::<{rd}>(pc_of(chain, h, d).wrapping_add({ret}u64));")
            out.append(f"    h.pc = pc_of(chain, h, d).wrapping_add({target}u64);")
            if rd in (1, 5):
                out.append(f"    ras_push_at(chain, h, d.add({k}));")
            out.append(f"    return finish_direct({CHAIN_ARG}tlb, h, bus, d.add({k}), (*d.add({k})).index as u64 + 1);")
            ended = True
            pc_off += x["len"]
            k += 1
            continue
        if op == "Jalr":
            # The target comes from the register as it is *now*: `jalr ra, ra, 0`
            # overwrites that register with the return address below.
            base = operand(rs1)
            ret = (pc_off + x["len"]) & 0xFFFF_FFFF_FFFF_FFFF
            if rd == rs1:
                t = f"t{nlocals}"
                nlocals += 1
                out.append(f"    let {t} = ({base}).wrapping_add({x['imm'] & 0xFFFF_FFFF_FFFF_FFFF}u64) & !1;")
                out.append(f"    h.set_x_c::<{rd}>(pc_of(chain, h, d).wrapping_add({ret}u64));")
                out.append(f"    h.pc = {t};")
            else:
                if rd != 0:
                    out.append(f"    h.set_x_c::<{rd}>(pc_of(chain, h, d).wrapping_add({ret}u64));")
                out.append(f"    h.pc = ({base}).wrapping_add({x['imm'] & 0xFFFF_FFFF_FFFF_FFFF}u64) & !1;")
            out.append(f"    return finish_jalr({CHAIN_ARG}tlb, h, bus, d.add({k}));")
            ended = True
            pc_off += x["len"]
            k += 1
            continue
        if op in LOADS:
            w, signed = LOADS[op]
            token = base_token(rs1)
            va = f"({operand(rs1)}).wrapping_add({x['imm'] & 0xFFFF_FFFF_FFFF_FFFF}u64)"
            hit = find_mem(token, x["imm"])
            if hit is not None and hit[3] >= w:
                v = f"v{nlocals}"
                nlocals += 1
                out.append(f"    let {v} = {narrowed(hit[2], w, signed)};")
                if rd != 0:
                    out.append(f"    h.set_x_c::<{rd}>({v});")
                    live[rd] = v
                    drop_base(("reg", rd))
                pc_off += x["len"]
                k += 1
                continue
            v = f"v{nlocals}"
            nlocals += 1
            out.append(f"    let {v} = match spec_load::<{w}, {str(signed).lower()}>(tlb, h, bus, {va}) {{")
            out.append("        Some(v) => v,")
            out.append("        None => {")
            out.append(f"            return spec_step_at({CHAIN_ARG}tlb, h, bus, d, {k});")
            out.append("        }")
            out.append("    };")
            if rd != 0:
                out.append(f"    h.set_x_c::<{rd}>({v});")
                live[rd] = v
            pc_off += x["len"]
            k += 1
            continue
        if op in STORES:
            w = STORES[op]
            va = f"({operand(rs1)}).wrapping_add({x['imm'] & 0xFFFF_FFFF_FFFF_FFFF}u64)"
            val = operand(rs2)
            out.append(f"    if !spec_store::<{w}>(tlb, h, bus, {va}, {val}) {{")
            out.append(f"        return spec_step_at({CHAIN_ARG}tlb, h, bus, d, {k});")
            out.append("    }")
            pc_off += x["len"]
            k += 1
            continue
        if rd == 0:
            # A pure write to x0 changes nothing.
            pc_off += x["len"]
            k += 1
            continue
        # `slli x, 32` + `srli x, 32` (and friends) is a 32-bit extension: the
        # intermediate is overwritten at once, so the pair becomes one value.
        folded = None
        if k + 1 < len(insns) and op in ("Slli", "Srli", "Srai") and rs1 == rd:
            nxt = insns[k + 1]
            if (
                nxt["rs1"] == rd
                and nxt["op"] in ("Slli", "Srli", "Srai")
                and (nxt["op"] == op or {op, nxt["op"]} == {"Slli", "Srli"})
                and rd not in reads_after[k + 1]
                and not any(y["op"] in MEM for y in insns[k + 1:k + 2])
            ):
                if x["imm"] + nxt["imm"] <= 63 and (x["imm"] == 0 or nxt["imm"] == 0
                                                     or (op == nxt["op"])):
                    folded = ("shift", op if x["imm"] else nxt["op"], x["imm"] + nxt["imm"])
                elif x["imm"] == 32 and nxt["imm"] == 32 and op == "Slli":
                    folded = ("ext", nxt["op"])
        if folded is not None:
            v = f"v{nlocals}"
            nlocals += 1
            src = operand(rs1)
            if folded[0] == "shift":
                kind, amt = folded[1], folded[2]
                out.append(f"    let {v} = alu::<{{ Op::{kind} as u8 }}>({src}, {amt}i64 as u64);")
            elif folded[1] == "Srli":
                out.append(f"    let {v} = ({src}) as u32 as u64;")
            else:
                out.append(f"    let {v} = sx32(({src}) as u32);")
            out.append(f"    h.set_x_c::<{rd}>({v});")
            live[rd] = v
            drop_base(("reg", rd))
            pc_off += x["len"] + insns[k + 1]["len"]
            out.append("    // folded the following shift")
            k += 2
            continue
        a = operand(rs1)
        if op in PURE:
            b = None
        elif op in ALU_IMM:
            b = f"{x['imm']}i64 as u64"
        else:
            b = operand(rs2)
        # A dead write can only be dropped when no memory instruction follows
        # before the overwrite: if one of those takes the slow path, the run is
        # abandoned there and the register must already hold its value.
        overwrite = next((j for j in range(k + 1, len(insns)) if insns[j]["rd"] == rd), None)
        dead = (
            rd not in reads_after[k]
            and overwrite is not None
            and all(insns[j]["op"] not in MEM for j in range(k + 1, overwrite))
        )
        if dead:
            pc_off += x["len"]
            k += 1
            continue
        # A move is a register copy: store the value, and let later instructions
        # in the run use it directly when it is already in a local.
        move_src = None
        if op == "Addi" and x["imm"] == 0:
            move_src = rs1
        elif op == "Andi" and x["imm"] == -1:
            move_src = rs1
        elif op in ("Ori", "Xori") and x["imm"] == 0:
            move_src = rs1
        elif op in ("Add", "Or", "Xor", "Sll", "Srl", "Sra", "And") and (rs1 == 0 or rs2 == 0):
            move_src = rs1 if rs2 == 0 else rs2
        elif op in ("Slli", "Srli", "Srai") and x["imm"] == 0:
            move_src = rs1
        if move_src is not None:
            v = operand(move_src)
            out.append(f"    h.set_x_c::<{rd}>({v});")
            if move_src in live:
                # A local is SSA: it keeps this value forever.
                live[rd] = live[move_src]
            elif not written_later(move_src):
                # Reading the source register again is fine only while nothing
                # writes it in this run.
                live[rd] = f"h.rx_c::<{move_src}>()"
            else:
                live.pop(rd, None)
            drop_base(("reg", rd))
            pc_off += x["len"]
            k += 1
            continue
        if op == "Lui":
            expr = f"{x['imm']}i64 as u64"
        elif op == "Auipc":
            expr = f"pc_of(chain, h, d).wrapping_add({(pc_off + x['imm']) & 0xFFFF_FFFF_FFFF_FFFF}u64)"
        else:
            expr = f"alu::<{{ Op::{op} as u8 }}>({a}, {b})"
        v = f"v{nlocals}"
        nlocals += 1
        out.append(f"    let {v} = {expr};")
        out.append(f"    h.set_x_c::<{rd}>({v});")
        live[rd] = v
        drop_base(("reg", rd))
        pc_off += x["len"]
        k += 1
    if not ended:
        out.append(f"    crate::counters::add(&crate::counters::SPEC_USE[{t}], 1);")
        out.append("    crate::counters::inc(&crate::counters::SPEC_UNITS);")
        out.append(f"    crate::counters::add(&crate::counters::SPEC_INSNS, {len(insns)});")
        out.append(f"    go_tlb({CHAIN_ARG}tlb, h, bus, d.add({len(insns)}), 0, 0)")
    out.append("}")
    out.append("")


def main():
    global OPS
    OPS = op_ordinals()
    runs_path, out_path = sys.argv[1], sys.argv[2]
    limit = int(sys.argv[3]) if len(sys.argv) > 3 else 96
    # Several profiles may be given, separated by colons. Each is scaled to the
    # same total, so a long workload does not drown out a short one.
    runs = []
    for path in runs_path.split(":"):
        part = [r for r in parse(path) if usable(r[1])]
        total = sum(c * len(i) for c, i in part) or 1
        runs.extend((c * 1e9 / total, i) for c, i in part)
    # Deduplicate, then rank by the number of instructions a template covers.
    best = {}
    for count, insns in runs:
        s = sig(insns)
        if s in best:
            best[s] = (best[s][0] + count, insns)
        else:
            best[s] = (count, insns)
    ranked = sorted(best.items(), key=lambda kv: kv[1][0] * len(kv[1][1]), reverse=True)[:limit]

    out = []
    out.append("//! Generated: specialised superinstructions for the hottest instruction")
    out.append("//! runs. Regenerate with `tools/gen_spec.py` from a counters build; the file")
    out.append("//! is checked in, so nothing depends on the profile at build time.")
    out.append("//!")
    out.append("//! Each function runs a fixed sequence of ALU instructions whose registers and")
    out.append("//! immediates are compile-time constants. Values that a later instruction in")
    out.append("//! the run reads are forwarded through a local, so the sequence touches the")
    out.append("//! register file only for what the outside world can see. Every instruction is")
    out.append("//! trap-free, so there is nothing to fall back to in the middle of a run.")
    out.append("")
    out.append("#![allow(clippy::too_many_arguments, unused_variables)]")
    out.append("")
    out.append("use crate::block::{alu, finish_jalr, finish_direct, go_tlb, pc_of, ras_push_at, spec_candidate, spec_load, spec_step_at, spec_store, spec_taken, Chain};")
    out.append("use crate::hart::Hart;")
    out.append("use crate::insn::{DInsn, Op, Step};")
    out.append("use crate::bus::Bus;")
    out.append("")
    covered = 0
    for t, (s, (count, insns)) in enumerate(ranked):
        emit_fn(t, insns, out)
        covered += count * len(insns)
    out.append("/// One generated superinstruction.")
    out.append("pub(crate) struct Spec {")
    out.append("    /// Packed opcode/register words of the sequence, two per instruction.")
    out.append("    pub(crate) words: &'static [u64],")
    out.append("    pub(crate) step: Step,")
    out.append("}")
    out.append("")
    out.append("/// The generated superinstructions, sorted by their first word so a block")
    out.append("/// build can find candidates with a binary search.")
    # Plain integer literals: a table of `Op::X as u64 | n << 8 | ...` expressions
    # makes rustc spend minutes in one giant constant. The enum's ordinals are
    # checked at compile time instead (see the asserts emitted below).
    table = []
    used_ops = set()
    for t, (s, (count, insns)) in enumerate(ranked):
        words = []
        for x in insns:
            used_ops.add(x["op"])
            words.append(f"0x{OPS[x['op']] | x['rd'] << 8 | x['rs1'] << 16 | x['rs2'] << 24:x}")
            words.append(f"0x{(x['imm'] & 0xFFFF_FFFF) | x['len'] << 32:x}")
        first = OPS[insns[0]["op"]] | int(insns[0]["rd"]) << 8 | int(insns[0]["rs1"]) << 16 | int(insns[0]["rs2"]) << 24
        table.append((first, t, len(words), ", ".join(words)))
    for name in sorted(used_ops):
        out.append(f"const _: () = assert!(Op::{name} as u64 == {OPS[name]});")
    out.append("")
    for _, t, nwords, lit in table:
        out.append(f"static W_{t}: [u64; {nwords}] = [{lit}];")
    out.append("")
    out.append("pub(crate) static SPECS: &[Spec] = &[")
    # `lookup` binary searches on the first word.
    for _, t, _, _ in sorted(table, key=lambda e: e[0]):
        out.append(f"    Spec {{ words: &W_{t}, step: spec_{t} }},")
    out.append("];")
    out.append("")
    # The filter in front of `lookup` is built at run time from SPECS (src/spec_index.rs).
    out.append("""/// Test builds can switch the generated steps off, so the same program can
/// run through the ordinary steps and the results be compared.
#[cfg(test)]
pub(crate) static DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[inline(always)]
pub(crate) const fn pack0(d: &DInsn) -> u64 {
    d.op as u64 | (d.rd as u64) << 8 | (d.rs1 as u64) << 16 | (d.rs2 as u64) << 24
}

#[inline(always)]
const fn pack1(d: &DInsn) -> u64 {
    (d.imm as u32) as u64 | (d.len as u64) << 32
}

/// The generated superinstruction that starts at `d`, if one matches. The
/// caller must be at a position where a run may start.
#[inline]
pub(crate) unsafe fn lookup(d: *const DInsn, limit: usize) -> Option<(usize, Step)> {
    #[cfg(test)]
    if DISABLED.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    // `RVEMU_NO_SPEC=1` runs the same program without any generated step. Every template
    // has at least three instructions (see `spec_index`).
    let ix = crate::spec_index::index();
    if ix.disabled || limit < 3 {
        return None;
    }
    let di = &*d;
    if !spec_candidate(di.op) {
        return None;
    }
    if di.rd == 0 && !crate::block::writes_register(di.op) {
        // A store: fine to start a run with.
    } else if di.rd == 0 && crate::block::fusible(di.op) {
        return None;
    }
    if !ix.may_match(d) {
        return None;
    }
    let first = pack0(di);
    let mut lo = 0usize;
    let mut hi = SPECS.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if SPECS[mid].words[0] < first {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut best: Option<(usize, Step)> = None;
    while lo < SPECS.len() && SPECS[lo].words[0] == first {
        let spec = &SPECS[lo];
        let n = spec.words.len() / 2;
        if n <= limit && best.map_or(true, |(blen, _)| n > blen) {
            let mut fits = true;
            for i in 0..n {
                let x = &*d.add(i);
                if x.op == Op::Empty || pack0(x) != spec.words[i * 2] || pack1(x) != spec.words[i * 2 + 1] {
                    fits = false;
                    break;
                }
            }
            if fits {
                best = Some((n, spec.step));
            }
        }
        lo += 1;
    }
    best
}""")
    open(out_path, "w").write("\n".join(out) + "\n")
    print(f"wrote {out_path}: {len(ranked)} templates covering about {covered/1e6:.1f}M instructions")


if __name__ == "__main__":
    main()
