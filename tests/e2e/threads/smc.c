/* Cross-modifying code (tests/e2e/threads.sh): one thread rewrites a function in an RWX page and
 * runs fence.i, another on a different hart calls it after an acquire; it must always see the
 * newest version. */
#include <pthread.h>
#include <stdio.h>
#include <stdint.h>
#include <sys/mman.h>
typedef int (*fn)(void);
static volatile uint32_t *code;
static volatile int seq, ack, bad, stop_;
#define N 4000
static void *writer(void *a){
  for (int k = 1; k <= N; k++) {
    int v = 1 + (k % 2000);
    code[0] = 0x00000513u | ((uint32_t)v << 20);   /* addi a0,zero,v */
    code[1] = 0x00008067u;                          /* ret */
    __builtin___clear_cache((char*)code, (char*)(code + 2));
    __atomic_store_n(&seq, k, __ATOMIC_RELEASE);
    while (__atomic_load_n(&ack, __ATOMIC_ACQUIRE) < k) ;
  }
  return 0;
}
static void *reader(void *a){
  for (int k = 1; k <= N; k++) {
    while (__atomic_load_n(&seq, __ATOMIC_ACQUIRE) < k) ;
    __builtin___clear_cache((char*)code, (char*)(code + 2));
    int r = ((fn)code)();
    if (r != 1 + (k % 2000)) bad++;
    __atomic_store_n(&ack, k, __ATOMIC_RELEASE);
  }
  return 0;
}
int main(void){
  code = mmap(0, 4096, PROT_READ|PROT_WRITE|PROT_EXEC, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0);
  code[0] = 0x00000513u | (1u<<20); code[1] = 0x00008067u;
  pthread_t w, r; pthread_create(&r, 0, reader, 0); pthread_create(&w, 0, writer, 0);
  pthread_join(w, 0); pthread_join(r, 0);
  printf("smc bad %d (want 0) %s\n", bad, bad ? "SMC_FAIL" : "SMC_OK");
  return 0;
}
