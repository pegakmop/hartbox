/* Concurrency torture for harts on separate host threads (tests/e2e/threads.sh).
 * Atomic add, CAS (lr/sc), an amoswap spinlock, 32-bit atomics, acquire/release message passing
 * and a store-buffering litmus with full fences: all counts must come out exact, (0,0) never. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#define N 4
#define ITER 400000
static volatile uint64_t atomic_ctr;
static uint64_t cas_ctr;
static volatile int lock_;
static uint64_t locked_ctr;
static uint32_t w32;
static int64_t minmax_hi = -1000000, minmax_lo = 1000000;
/* message passing */
static uint64_t data[8];
static volatile int flag;
static volatile int mp_bad;
/* ticket-free spinlock via amoswap */
static void lk(void){ while (__atomic_exchange_n(&lock_, 1, __ATOMIC_ACQUIRE)) { while (lock_) ; } }
static void ul(void){ __atomic_store_n(&lock_, 0, __ATOMIC_RELEASE); }
static void *worker(void *arg){
  long id = (long)arg;
  for (int i = 0; i < ITER; i++) {
    __atomic_fetch_add(&atomic_ctr, 1, __ATOMIC_SEQ_CST);
    uint64_t o = __atomic_load_n(&cas_ctr, __ATOMIC_RELAXED);
    while (!__atomic_compare_exchange_n(&cas_ctr, &o, o + 1, 0, __ATOMIC_SEQ_CST, __ATOMIC_RELAXED)) ;
    lk(); locked_ctr++; ul();
    __atomic_fetch_add(&w32, 3, __ATOMIC_RELAXED);
    int64_t v = (int64_t)((i * 7 + id * 13) % 1000);
    int64_t cur = __atomic_load_n(&minmax_hi, __ATOMIC_RELAXED);
    while (v > cur && !__atomic_compare_exchange_n(&minmax_hi, &cur, v, 0, __ATOMIC_SEQ_CST, __ATOMIC_RELAXED)) ;
  }
  return 0;
}
static void *producer(void *arg){
  for (int r = 1; r <= 200000; r++) {
    while (__atomic_load_n(&flag, __ATOMIC_ACQUIRE) != 0) ;
    for (int k = 0; k < 8; k++) data[k] = r * 8 + k;
    __atomic_store_n(&flag, 1, __ATOMIC_RELEASE);
  }
  return 0;
}
static void *consumer(void *arg){
  for (int r = 1; r <= 200000; r++) {
    while (__atomic_load_n(&flag, __ATOMIC_ACQUIRE) != 1) ;
    for (int k = 0; k < 8; k++) if (data[k] != (uint64_t)r * 8 + k) mp_bad++;
    __atomic_store_n(&flag, 0, __ATOMIC_RELEASE);
  }
  return 0;
}

/* store-buffering litmus: with a full fence between the store and the load, (0,0) is forbidden */
static volatile int x, y, r0v, r1v, go, done_cnt;
#define LIT 150000
static void *lt0(void *a){ for (int i=0;i<LIT;i++){ while(__atomic_load_n(&go,__ATOMIC_ACQUIRE)<2*i+1); x=1; __atomic_thread_fence(__ATOMIC_SEQ_CST); r0v=y; __atomic_fetch_add(&done_cnt,1,__ATOMIC_SEQ_CST); while(__atomic_load_n(&go,__ATOMIC_ACQUIRE)<2*i+2);} return 0; }
static void *lt1(void *a){ for (int i=0;i<LIT;i++){ while(__atomic_load_n(&go,__ATOMIC_ACQUIRE)<2*i+1); y=1; __atomic_thread_fence(__ATOMIC_SEQ_CST); r1v=x; __atomic_fetch_add(&done_cnt,1,__ATOMIC_SEQ_CST); while(__atomic_load_n(&go,__ATOMIC_ACQUIRE)<2*i+2);} return 0; }
static int sb_bad;
static void litmus(void){
  pthread_t a,b; pthread_create(&a,0,lt0,0); pthread_create(&b,0,lt1,0);
  for (int i=0;i<LIT;i++){ x=0; y=0; done_cnt=0; __atomic_store_n(&go,2*i+1,__ATOMIC_RELEASE);
    while(__atomic_load_n(&done_cnt,__ATOMIC_ACQUIRE)!=2); if (r0v==0 && r1v==0) sb_bad++; __atomic_store_n(&go,2*i+2,__ATOMIC_RELEASE); }
  pthread_join(a,0); pthread_join(b,0);
}
int main(void){
  pthread_t t[N];
  for (long i = 0; i < N; i++) pthread_create(&t[i], 0, worker, (void*)i);
  for (int i = 0; i < N; i++) pthread_join(t[i], 0);
  printf("atomic %llu (want %d)\n", (unsigned long long)atomic_ctr, N*ITER);
  printf("cas    %llu (want %d)\n", (unsigned long long)cas_ctr, N*ITER);
  printf("locked %llu (want %d)\n", (unsigned long long)locked_ctr, N*ITER);
  printf("w32    %u (want %u)\n", w32, 3u*N*ITER);
  printf("max    %lld (want 999)\n", (long long)minmax_hi);
  pthread_t p, c;
  pthread_create(&p, 0, producer, 0); pthread_create(&c, 0, consumer, 0);
  pthread_join(p, 0); pthread_join(c, 0);
  printf("mp_bad %d (want 0)\n", mp_bad);
  litmus(); printf("sb_bad %d (want 0)\n", sb_bad);
  puts(sb_bad ? "TORTURE_FAIL" : atomic_ctr == N*ITER && cas_ctr == N*ITER && locked_ctr == N*ITER && w32 == 3u*N*ITER && !mp_bad ? "TORTURE_OK" : "TORTURE_FAIL");
  return 0;
}
