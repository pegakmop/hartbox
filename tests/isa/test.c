// Bare-metal differential test: prints the results of many instructions over
// edge-case inputs. The same ELF runs on QEMU and rvemu; the outputs must match.
#include <stdint.h>

#define UART ((volatile uint8_t *)0x10000000)
#define FINISHER ((volatile uint32_t *)0x100000)

static void putc_(char c) { *UART = (uint8_t)c; }
static void puts_(const char *s) { while (*s) putc_(*s++); }
static void puthex(uint64_t v) {
    for (int i = 15; i >= 0; i--) {
        int d = (v >> (i * 4)) & 0xf;
        putc_(d < 10 ? '0' + d : 'a' + d - 10);
    }
    putc_(' ');
}
static void nl(void) { putc_('\n'); }
static void finish(uint32_t code) {
    *FINISHER = code ? ((code << 16) | 0x3333) : 0x5555;
    for (;;) {
    }
}

static volatile uint64_t ints[] = {
    0, 1, 2, 3, 0x7f, 0x80, 0xff, 0x7fffffff, 0x80000000ull, 0xffffffffull,
    0x7fffffffffffffffull, 0x8000000000000000ull, 0xffffffffffffffffull,
    0x123456789abcdef0ull, 0xfedcba9876543210ull, 5, (uint64_t)-5, 1000000007ull,
    0x00000000fffffffeull, 0xffffffff80000000ull,
};
#define NI ((int)(sizeof(ints) / sizeof(ints[0])))

static inline uint64_t mulh_(uint64_t a, uint64_t b) {
    uint64_t r;
    asm("mulh %0, %1, %2" : "=r"(r) : "r"(a), "r"(b));
    return r;
}
static inline uint64_t mulhu_(uint64_t a, uint64_t b) {
    uint64_t r;
    asm("mulhu %0, %1, %2" : "=r"(r) : "r"(a), "r"(b));
    return r;
}
static inline uint64_t mulhsu_(uint64_t a, uint64_t b) {
    uint64_t r;
    asm("mulhsu %0, %1, %2" : "=r"(r) : "r"(a), "r"(b));
    return r;
}

#define INT2(name, expr)                                                   \
    for (int i = 0; i < NI; i++)                                           \
        for (int j = 0; j < NI; j++) {                                     \
            uint64_t a = ints[i], b = ints[j];                             \
            uint64_t r = (expr);                                           \
            puts_(name " ");                                               \
            puthex(a);                                                     \
            puthex(b);                                                     \
            puthex(r);                                                     \
            nl();                                                          \
        }

static void int_tests(void) {
    INT2("add", a + b);
    INT2("sub", a - b);
    INT2("sll", a << (b & 63));
    INT2("srl", a >> (b & 63));
    INT2("sra", (uint64_t)((int64_t)a >> (b & 63)));
    INT2("slt", (uint64_t)((int64_t)a < (int64_t)b));
    INT2("sltu", (uint64_t)(a < b));
    INT2("xor", a ^ b);
    INT2("or", a | b);
    INT2("and", a & b);
    INT2("mul", a * b);
    INT2("mulh", mulh_(a, b));
    INT2("mulhu", mulhu_(a, b));
    INT2("mulhsu", mulhsu_(a, b));
    INT2("divu", b ? a / b : ~0ull);
    INT2("remu", b ? a % b : a);
    INT2("addw", (uint64_t)(int64_t)(int32_t)((uint32_t)a + (uint32_t)b));
    INT2("subw", (uint64_t)(int64_t)(int32_t)((uint32_t)a - (uint32_t)b));
    INT2("sllw", (uint64_t)(int64_t)(int32_t)((uint32_t)a << (b & 31)));
    INT2("srlw", (uint64_t)(int64_t)(int32_t)((uint32_t)a >> (b & 31)));
    INT2("sraw", (uint64_t)(int64_t)((int32_t)a >> (b & 31)));
    INT2("mulw", (uint64_t)(int64_t)(int32_t)((uint32_t)a * (uint32_t)b));
    INT2("divuw", (uint32_t)b ? (uint64_t)(int64_t)(int32_t)((uint32_t)a / (uint32_t)b) : ~0ull);
    INT2("remuw", (uint32_t)b ? (uint64_t)(int64_t)(int32_t)((uint32_t)a % (uint32_t)b) : (uint64_t)(int64_t)(int32_t)(uint32_t)a);
}

// Signed division with the RISC-V edge-case rules, written in asm so the
// compiler cannot fold it.
static inline uint64_t div_(uint64_t a, uint64_t b) {
    uint64_t r;
    asm("div %0, %1, %2" : "=r"(r) : "r"(a), "r"(b));
    return r;
}
static inline uint64_t rem_(uint64_t a, uint64_t b) {
    uint64_t r;
    asm("rem %0, %1, %2" : "=r"(r) : "r"(a), "r"(b));
    return r;
}
static inline uint64_t divw_(uint64_t a, uint64_t b) {
    uint64_t r;
    asm("divw %0, %1, %2" : "=r"(r) : "r"(a), "r"(b));
    return r;
}
static inline uint64_t remw_(uint64_t a, uint64_t b) {
    uint64_t r;
    asm("remw %0, %1, %2" : "=r"(r) : "r"(a), "r"(b));
    return r;
}

static void div_tests(void) {
    INT2("div", div_(a, b));
    INT2("rem", rem_(a, b));
    INT2("divw", divw_(a, b));
    INT2("remw", remw_(a, b));
}

// ----------------------------------------------------------- floating point
static volatile uint64_t fbits[] = {
    0x0000000000000000ull, 0x8000000000000000ull, 0x3ff0000000000000ull,
    0xbff0000000000000ull, 0x3fb999999999999aull, 0x400c000000000000ull,
    0x7fe1ccf385ebc8a0ull, 0x0010000000000000ull, 0x0000000000000001ull,
    0x8000000000000001ull, 0x7ff0000000000000ull, 0xfff0000000000000ull,
    0x7ff8000000000000ull, 0x7ff0000000000001ull, 0x4340000000000001ull,
    0x4330000000000000ull, 0x3ff8000000000001ull, 0x3ff0000000000002ull,
    0x4000000000000000ull, 0x3fe0000000000000ull,
};
#define NF ((int)(sizeof(fbits) / sizeof(fbits[0])))

static uint64_t fflags_take(void) {
    uint64_t f;
    asm volatile("csrr %0, fflags" : "=r"(f));
    asm volatile("csrw fflags, zero");
    return f;
}

static inline void set_frm(uint64_t rm) { asm volatile("csrw frm, %0" ::"r"(rm)); }

static inline uint64_t fop2d(int op, uint64_t a, uint64_t b) {
    uint64_t r;
    double x, y, z;
    asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(a));
    asm volatile("fmv.d.x %0, %1" : "=f"(y) : "r"(b));
    switch (op) {
    case 0: asm volatile("fadd.d %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    case 1: asm volatile("fsub.d %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    case 2: asm volatile("fmul.d %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    default: asm volatile("fdiv.d %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    }
    asm volatile("fmv.x.d %0, %1" : "=r"(r) : "f"(z));
    return r;
}

static inline uint64_t fop2s(int op, uint64_t a, uint64_t b) {
    uint64_t r;
    float x, y, z;
    asm volatile("fmv.w.x %0, %1" : "=f"(x) : "r"((uint32_t)a));
    asm volatile("fmv.w.x %0, %1" : "=f"(y) : "r"((uint32_t)b));
    switch (op) {
    case 0: asm volatile("fadd.s %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    case 1: asm volatile("fsub.s %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    case 2: asm volatile("fmul.s %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    default: asm volatile("fdiv.s %0, %1, %2, dyn" : "=f"(z) : "f"(x), "f"(y)); break;
    }
    asm volatile("fmv.x.w %0, %1" : "=r"(r) : "f"(z));
    return r & 0xffffffffull;
}

static uint64_t fsqrt_d(uint64_t a) {
    double x, z;
    uint64_t r;
    asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(a));
    asm volatile("fsqrt.d %0, %1, dyn" : "=f"(z) : "f"(x));
    asm volatile("fmv.x.d %0, %1" : "=r"(r) : "f"(z));
    return r;
}

static uint64_t fmadd_d(uint64_t a, uint64_t b, uint64_t c) {
    double x, y, w, z;
    uint64_t r;
    asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(a));
    asm volatile("fmv.d.x %0, %1" : "=f"(y) : "r"(b));
    asm volatile("fmv.d.x %0, %1" : "=f"(w) : "r"(c));
    asm volatile("fmadd.d %0, %1, %2, %3, dyn" : "=f"(z) : "f"(x), "f"(y), "f"(w));
    asm volatile("fmv.x.d %0, %1" : "=r"(r) : "f"(z));
    return r;
}

static uint64_t fcmp_d(uint64_t a, uint64_t b, int kind) {
    double x, y, z;
    uint64_t r;
    asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(a));
    asm volatile("fmv.d.x %0, %1" : "=f"(y) : "r"(b));
    if (kind == 0) asm volatile("feq.d %0, %1, %2" : "=r"(r) : "f"(x), "f"(y));
    else if (kind == 1) asm volatile("flt.d %0, %1, %2" : "=r"(r) : "f"(x), "f"(y));
    else if (kind == 2) asm volatile("fle.d %0, %1, %2" : "=r"(r) : "f"(x), "f"(y));
    else if (kind == 3) {
        asm volatile("fmin.d %0, %1, %2" : "=f"(z) : "f"(x), "f"(y));
        asm volatile("fmv.x.d %0, %1" : "=r"(r) : "f"(z));
    } else {
        asm volatile("fmax.d %0, %1, %2" : "=f"(z) : "f"(x), "f"(y));
        asm volatile("fmv.x.d %0, %1" : "=r"(r) : "f"(z));
    }
    return r;
}

static uint64_t fcvt_w_d(uint64_t a, int kind) {
    double x;
    int64_t r;
    asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(a));
    switch (kind) {
    case 0: asm volatile("fcvt.w.d %0, %1, dyn" : "=r"(r) : "f"(x)); break;
    case 1: asm volatile("fcvt.wu.d %0, %1, dyn" : "=r"(r) : "f"(x)); break;
    case 2: asm volatile("fcvt.l.d %0, %1, dyn" : "=r"(r) : "f"(x)); break;
    default: asm volatile("fcvt.lu.d %0, %1, dyn" : "=r"(r) : "f"(x)); break;
    }
    return (uint64_t)r;
}

static uint64_t fcvt_d_int(uint64_t a, int kind) {
    double z;
    uint64_t r;
    switch (kind) {
    case 0: asm volatile("fcvt.d.w %0, %1, dyn" : "=f"(z) : "r"(a)); break;
    case 1: asm volatile("fcvt.d.wu %0, %1, dyn" : "=f"(z) : "r"(a)); break;
    case 2: asm volatile("fcvt.d.l %0, %1, dyn" : "=f"(z) : "r"(a)); break;
    default: asm volatile("fcvt.d.lu %0, %1, dyn" : "=f"(z) : "r"(a)); break;
    }
    asm volatile("fmv.x.d %0, %1" : "=r"(r) : "f"(z));
    return r;
}

static uint64_t fcvt_s_d(uint64_t a) {
    double x;
    float z;
    uint64_t r;
    asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(a));
    asm volatile("fcvt.s.d %0, %1, dyn" : "=f"(z) : "f"(x));
    asm volatile("fmv.x.w %0, %1" : "=r"(r) : "f"(z));
    return r & 0xffffffffull;
}

static uint64_t fclass_d(uint64_t a) {
    double x;
    uint64_t r;
    asm volatile("fmv.d.x %0, %1" : "=f"(x) : "r"(a));
    asm volatile("fclass.d %0, %1" : "=r"(r) : "f"(x));
    return r;
}

static void fp_tests(void) {
    static const char *rmname[] = {"rne", "rtz", "rdn", "rup", "rmm"};
    for (int rm = 0; rm < 5; rm++) {
        set_frm(rm);
        for (int op = 0; op < 4; op++)
            for (int i = 0; i < NF; i++)
                for (int j = 0; j < NF; j++) {
                    uint64_t a = fbits[i], b = fbits[j];
                    uint64_t r = fop2d(op, a, b);
                    uint64_t fl = fflags_take();
                    puts_(rmname[rm]);
                    puts_(" d");
                    putc_('0' + op);
                    puthex(r);
                    puthex(fl);
                    nl();
                    r = fop2s(op, a, b);
                    fl = fflags_take();
                    puts_(rmname[rm]);
                    puts_(" s");
                    putc_('0' + op);
                    puthex(r);
                    puthex(fl);
                    nl();
                }
        for (int i = 0; i < NF; i++) {
            uint64_t r = fsqrt_d(fbits[i]);
            uint64_t fl = fflags_take();
            puts_(rmname[rm]);
            puts_(" sqrt ");
            puthex(r);
            puthex(fl);
            nl();
            r = fcvt_s_d(fbits[i]);
            fl = fflags_take();
            puts_(rmname[rm]);
            puts_(" cvt.s.d ");
            puthex(r);
            puthex(fl);
            nl();
            for (int k = 0; k < 4; k++) {
                r = fcvt_w_d(fbits[i], k);
                fl = fflags_take();
                puts_(rmname[rm]);
                puts_(" cvt.int ");
                putc_('0' + k);
                putc_(' ');
                puthex(r);
                puthex(fl);
                nl();
            }
        }
        for (int i = 0; i < NI; i++)
            for (int k = 0; k < 4; k++) {
                uint64_t r = fcvt_d_int(ints[i], k);
                uint64_t fl = fflags_take();
                puts_(rmname[rm]);
                puts_(" cvt.from ");
                putc_('0' + k);
                putc_(' ');
                puthex(r);
                puthex(fl);
                nl();
            }
        for (int i = 0; i < NF; i++)
            for (int j = 0; j < NF; j++) {
                uint64_t a = fbits[i], b = fbits[j];
                uint64_t r = fmadd_d(a, b, fbits[(i + j) % NF]);
                uint64_t fl = fflags_take();
                puts_(rmname[rm]);
                puts_(" fmadd ");
                puthex(r);
                puthex(fl);
                nl();
            }
    }
    set_frm(0);
    for (int i = 0; i < NF; i++)
        for (int j = 0; j < NF; j++) {
            for (int k = 0; k < 5; k++) {
                uint64_t r = fcmp_d(fbits[i], fbits[j], k);
                uint64_t fl = fflags_take();
                puts_("cmp ");
                putc_('0' + k);
                putc_(' ');
                puthex(r);
                puthex(fl);
                nl();
            }
        }
    for (int i = 0; i < NF; i++) {
        uint64_t r = fclass_d(fbits[i]);
        puts_("class ");
        puthex(r);
        nl();
    }
}

// ------------------------------------------------------------ atomics, traps
static volatile uint64_t cell;
static volatile uint32_t cell32;

static volatile uint64_t trap_cause, trap_tval, trap_pc_hit;

static void trap_handler_stub(void);
asm(".align 4\n"
    "trap_handler_stub:\n"
    "  csrr t0, mcause\n"
    "  la t1, trap_cause\n"
    "  sd t0, 0(t1)\n"
    "  csrr t0, mtval\n"
    "  la t1, trap_tval\n"
    "  sd t0, 0(t1)\n"
    "  csrr t0, mepc\n"
    "  addi t0, t0, 4\n"
    "  csrw mepc, t0\n"
    "  mret\n");

static void atomic_tests(void) {
    cell = 10;
    uint64_t old;
    asm volatile("amoadd.d %0, %2, (%1)" : "=r"(old) : "r"(&cell), "r"((uint64_t)5));
    puts_("amoadd "); puthex(old); puthex(cell); nl();
    asm volatile("amoswap.d %0, %2, (%1)" : "=r"(old) : "r"(&cell), "r"((uint64_t)77));
    puts_("amoswap "); puthex(old); puthex(cell); nl();
    cell32 = 0xfffffff0u;
    uint64_t o32;
    asm volatile("amomax.w %0, %2, (%1)" : "=r"(o32) : "r"(&cell32), "r"((uint64_t)7));
    puts_("amomax.w "); puthex(o32); puthex(cell32); nl();
    asm volatile("amominu.w %0, %2, (%1)" : "=r"(o32) : "r"(&cell32), "r"((uint64_t)7));
    puts_("amominu.w "); puthex(o32); puthex(cell32); nl();
    // LR/SC success and failure
    uint64_t v, status;
    asm volatile("lr.d %0, (%1)" : "=r"(v) : "r"(&cell));
    asm volatile("sc.d %0, %2, (%1)" : "=r"(status) : "r"(&cell), "r"((uint64_t)123));
    puts_("lrsc ok "); puthex(v); puthex(status); puthex(cell); nl();
    asm volatile("sc.d %0, %2, (%1)" : "=r"(status) : "r"(&cell), "r"((uint64_t)999));
    puts_("sc fail "); puthex(status); puthex(cell); nl();
    // Misaligned access within RAM
    static volatile uint8_t buf[32] __attribute__((aligned(16)));
    for (int i = 0; i < 32; i++) buf[i] = (uint8_t)(i * 7 + 1);
    uint64_t m;
    asm volatile("ld %0, 0(%1)" : "=r"(m) : "r"(buf + 1));
    puts_("misaligned ld "); puthex(m); nl();
    // Traps
    asm volatile("csrw mtvec, %0" ::"r"(&trap_handler_stub));
    asm volatile("ecall");
    puts_("ecall cause="); puthex(trap_cause); nl();
    asm volatile("ebreak");
    puts_("ebreak cause="); puthex(trap_cause); nl();
    asm volatile(".word 0");
    puts_("illegal cause="); puthex(trap_cause); puthex(trap_tval); nl();
    uint64_t misa;
    asm volatile("csrr %0, misa" : "=r"(misa));
    puts_("misa "); puthex(misa); nl();
    // Unaligned store-load round trip
    asm volatile("sd %0, 0(%1)" ::"r"(0x0102030405060708ull), "r"(buf + 3));
    uint64_t back;
    asm volatile("ld %0, 0(%1)" : "=r"(back) : "r"(buf + 3));
    puts_("roundtrip "); puthex(back); nl();
}

int main(void) {
    // Enable the FP unit explicitly: reset state differs between emulators.
    asm volatile("li t0, 0x2000\n csrs mstatus, t0" ::: "t0");
    int_tests();
    div_tests();
    fp_tests();
    atomic_tests();
    puts_("done\n");
    finish(0);
    return 0;
}

void __attribute__((section(".text.init"), naked)) _start(void) {
    asm volatile(
        "la sp, _stack_top\n"
        "call main\n"
        "1: j 1b\n");
}
