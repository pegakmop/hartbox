/* Search one byte stream. Build with -DCOMPARE to compare two streams.
 * Both modes verify the exact exit position and the values that stopped them.
 */
.section .text.init, "ax"
.globl _start
_start:
    la      s0, first
    la      s1, second
    li      s2, (4 << 20)
    add     s3, s0, s2
    add     s4, s1, s2
    li      s5, 1
    li      s6, 48
#ifdef COMPARE
    sb      s5, -1(s4)
#else
    sb      s5, -1(s3)
#endif
1:
    mv      a0, s0
#ifdef COMPARE
    mv      a1, s1
2:
    lbu     t0, 0(a0)
    addi    a0, a0, 1
    lbu     t1, 0(a1)
    addi    a1, a1, 1
    beq     t0, t1, 2b
    bnez    t0, 3f
    bne     t1, s5, 3f
    bne     a1, s4, 3f
#else
2:
    lbu     t0, 0(a0)
    addi    a0, a0, 1
    bne     t0, s5, 2b
    bne     t0, s5, 3f
#endif
    bne     a0, s3, 3f
    addi    s6, s6, -1
    bnez    s6, 1b
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
ok:  .asciz "STREAM_OK\n"
bad: .asciz "STREAM_BAD\n"
.section .bss
.balign 4096
first:  .zero (4 << 20)
second: .zero (4 << 20)
