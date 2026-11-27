/* Allocation loop used by the hook-cost benchmark.
 *
 * Every iteration allocates and frees one 64-byte block through malloc/free, so
 * the wall time difference between a plain run and a run with a uprobe attached
 * is the cost of the probe itself. Kept dynamically linked on purpose: bpftime
 * rewrites the PLT, which a static binary does not have.
 *
 *   victim <iterations> [warmup_seconds]
 */
#include <stdio.h>
#include <stdlib.h>
#include <time.h>

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

int main(int argc, char **argv) {
    long iterations = argc > 1 ? atol(argv[1]) : 1000000;
    double warmup = argc > 2 ? atof(argv[2]) : 0.0;
    if (warmup > 0) {
        struct timespec ts = {(time_t)warmup, 0};
        nanosleep(&ts, NULL);
    }
    double started = now_ms();
    volatile long sink = 0;
    for (long index = 0; index < iterations; index++) {
        unsigned char *block = malloc(64);
        if (!block) return 1;
        block[0] = (unsigned char)index;
        sink += block[0];
        free(block);
    }
    double elapsed = now_ms() - started;
    printf("iterations=%ld wall_ms=%.1f ns_per_alloc_free=%.0f sink=%ld\n",
           iterations, elapsed, elapsed * 1e6 / iterations / 2, sink);
    return 0;
}
