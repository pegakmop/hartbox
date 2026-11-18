/* Build with -DCOPY for RAM copies or without it for fills. Use -DWORD_FILL
 * to fill with a value whose bytes are different. With -DCOPY, use -DREVERSE
 * for a descending copy or -DSENTINEL for a copy that includes its marker.
 * Use -DREDUCE to sum a RAM stream.
 * The working sets and iteration counts are identical across emulator builds.
 */
.section .text.init, "ax"
.globl _start
_start:
    la      s0, source
    la      s1, destination
    li      t0, (8 << 20)
    add     s2, s0, t0
    add     s3, s1, t0
#if defined(SENTINEL) || defined(REDUCE)
    li      s4, 0x123456789abcdef0
    sd      s4, -8(s2)
#else
#ifdef WORD_FILL
    li      s4, 0x123456789abcdef0
#else
    li      s4, 0x7575757575757575
#endif
#endif
#ifdef REVERSE
    sd      s4, 0(s0)
    addi    s6, s0, -8
#endif
    li      s5, 192
1:
#if defined(REDUCE)
    mv      a0, s0
    li      t4, 0
2:
    ld      t0, 0(a0)
    ld      t1, 8(a0)
    ld      t2, 16(a0)
    ld      t3, 24(a0)
    add     t4, t4, t0
    add     t4, t4, t1
    add     t4, t4, t2
    add     t4, t4, t3
    addi    a0, a0, 32
    bltu    a0, s2, 2b
    bne     t4, s4, 3f
#elif defined(COPY)
#if defined(REVERSE)
    addi    a0, s2, -8
    addi    a1, s3, -8
2:
    .set    offset, 0
    .rept   8
    ld      t0, offset(a0)
    sd      t0, offset(a1)
    .set    offset, offset - 8
    .endr
    addi    a0, a0, -64
    addi    a1, a1, -64
    bne     a0, s6, 2b
    ld      t0, 0(s1)
    bne     t0, s4, 3f
#elif defined(SENTINEL)
    mv      a0, s0
    mv      a1, s1
2:
    ld      t0, 0(a0)
    sd      t0, 0(a1)
    addi    a0, a0, 8
    addi    a1, a1, 8
    bne     t0, s4, 2b
    bne     a0, s2, 3f
    bne     a1, s3, 3f
    ld      t0, -8(a1)
    bne     t0, s4, 3f
#else
    mv      a0, s0
    mv      a1, s1
2:
    ld      t0, 0(a0)
    ld      t1, 8(a0)
    ld      t2, 16(a0)
    ld      t3, 24(a0)
    sd      t0, 0(a1)
    sd      t1, 8(a1)
    sd      t2, 16(a1)
    sd      t3, 24(a1)
    ld      t0, 32(a0)
    ld      t1, 40(a0)
    ld      t2, 48(a0)
    ld      t3, 56(a0)
    addi    a0, a0, 64
    sd      t0, 32(a1)
    sd      t1, 40(a1)
    sd      t2, 48(a1)
    sd      t3, 56(a1)
    addi    a1, a1, 64
    bltu    a0, s2, 2b
    ld      t0, -8(a1)
    bnez    t0, 3f
#endif
#else
    mv      a1, s1
2:
    sd      s4, 0(a1)
    sd      s4, 8(a1)
    sd      s4, 16(a1)
    sd      s4, 24(a1)
    sd      s4, 32(a1)
    sd      s4, 40(a1)
    sd      s4, 48(a1)
    sd      s4, 56(a1)
    addi    a1, a1, 64
    bltu    a1, s3, 2b
    ld      t0, -8(a1)
    bne     t0, s4, 3f
#endif
    addi    s5, s5, -1
    bnez    s5, 1b
    la      a0, ok
    j       4f
3:
    la      a0, bad
4:
    li      t0, 0x10000000
5:
    lbu     t1, 0(a0)
    beqz    t1, 6f
    sb      t1, 0(t0)
    addi    a0, a0, 1
    j       5b
6:
    li      t0, 0x100000
    li      t1, 0x5555
    sw      t1, 0(t0)
7:  j       7b
.section .rodata
ok:  .asciz "BULK_OK\n"
bad: .asciz "BULK_BAD\n"
.section .bss
.balign 4096
source: .zero (8 << 20)
.balign 4096
destination: .zero (8 << 20)
