#include <stdint.h>
#define UART ((volatile uint8_t *)0x10000000)
#define FINISHER ((volatile uint32_t *)0x100000)
static void puts_(const char *s) { while (*s) *UART = (uint8_t)*s++; }
static void puthex(uint64_t v) {
    for (int i = 15; i >= 0; i--) { int d = (v >> (i * 4)) & 0xf; *UART = d < 10 ? '0' + d : 'a' + d - 10; }
    *UART = '\n';
}
static uint64_t arr[1024];
static uint64_t work(uint64_t n) {
    uint64_t x = 1, acc = 0;
    for (uint64_t i = 0; i < n; i++) {
        x = x * 6364136223846793005ULL + 1442695040888963407ULL;
        uint64_t idx = (x >> 20) & 1023;
        arr[idx] += x;
        acc ^= arr[(i * 7) & 1023] + (x >> 3);
        if (acc & 1) acc += i; else acc -= i >> 2;
    }
    return acc;
}
static uint64_t fib(uint32_t n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
int main(void) {
    puts_("bench start\n");
    puthex(work(30000000));
    puthex(fib(30));
    puts_("bench end\n");
    *FINISHER = 0x5555;
    for (;;) {}
}
void __attribute__((section(".text.init"), naked)) _start(void) {
    asm volatile("la sp, _stack_top\n call main\n 1: j 1b\n");
}
