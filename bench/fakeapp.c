// fakeapp — synthetic "desktop app" for memory-offload benchmarks.
//
//   fakeapp <total_mb> <hot_pct> <warm_pct> <out_file>
//
// Allocates <total_mb> of anonymous memory filled with text-like data
// (compresses roughly like real heap memory), then:
//   hot  set  — every page read once per second   (what the user is doing now)
//   warm set  — every page read once per 90 s     (tabs/chats you glance at)
//   cold set  — never touched                     (what an offloader should take)
// Each sweep is timed; a sweep that has to fault pages back from zram gets slower.
// On SIGUSR1 the cold set is read once ("switching back to the app"), that time is
// recorded, stats are written to <out_file>, and the process exits.
#define _GNU_SOURCE
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#define PG 4096UL
static volatile sig_atomic_t stop_flag = 0;
static void on_usr1(int s) { (void)s; stop_flag = 1; }

static double now_ms(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}
static uint64_t rs = 88172645463325252ULL;
static inline uint64_t xr(void) { rs ^= rs << 13; rs ^= rs >> 7; rs ^= rs << 17; return rs; }

static const char *W[] = {"the","memory","page","function","return","value","object","string","array",
  "window","event","render","style","message","user","session","token","cache","buffer","request",
  "response","json","layout","script","handler","promise","callback","element","node","index"};

static void fill(char *p, size_t n) {
    size_t i = 0;
    while (i < n) {
        const char *w = W[xr() % 30];
        size_t l = strlen(w);
        if (xr() % 4 == 0) { uint64_t r = xr(); for (int k = 0; k < 6 && i < n; k++) p[i++] = 33 + (r >> (k * 8)) % 90; }
        for (size_t k = 0; k < l && i < n; k++) p[i++] = w[k];
        if (i < n) p[i++] = ' ';
    }
}
static volatile uint64_t sink;
static double sweep(char *base, size_t pages) {
    double t = now_ms(); uint64_t s = 0;
    for (size_t i = 0; i < pages; i++) s += (unsigned char)base[i * PG + (i % 64) * 8];
    sink += s; return now_ms() - t;
}
static int cmp(const void *a, const void *b) { double x = *(double *)a, y = *(double *)b; return x < y ? -1 : x > y; }
static double q(double *v, size_t n, double p) { if (!n) return 0; return v[(size_t)((n - 1) * p)]; }

int main(int argc, char **argv) {
    if (argc < 5) { fprintf(stderr, "usage: fakeapp MB HOT%% WARM%% OUT\n"); return 1; }
    size_t mb = atol(argv[1]); int hp = atoi(argv[2]), wp = atoi(argv[3]);
    size_t pages = mb * 256, hot = pages * hp / 100, warm = pages * wp / 100, cold = pages - hot - warm;
    rs ^= (uint64_t)getpid() * 2654435761ULL;
    char *m = mmap(NULL, pages * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) { perror("mmap"); return 1; }
    madvise(m, pages * PG, MADV_NOHUGEPAGE);  // 4K pages, like most of a real app heap
    double tf = now_ms();
    fill(m, pages * PG);
    double fill_ms = now_ms() - tf;
    { char rp[512]; snprintf(rp, sizeof rp, "%s.ready", argv[4]); FILE *rf = fopen(rp, "w"); if (rf) { fprintf(rf, "fill_ms=%.0f\n", fill_ms); fclose(rf); } }
    signal(SIGUSR1, on_usr1);
    char *H = m, *Wm = m + hot * PG, *C = m + (hot + warm) * PG;

    size_t cap = 1 << 16, nh = 0, nw = 0; double *hs = malloc(cap * sizeof(double)), *ws = malloc(4096 * sizeof(double));
    double tw = now_ms();
    while (!stop_flag) {
        double d = sweep(H, hot); if (nh < cap) hs[nh++] = d;
        if (now_ms() - tw >= 90000) { tw = now_ms(); double e = sweep(Wm, warm); if (nw < 4096) ws[nw++] = e; }
        usleep(1000000);
    }
    struct rusage r0; getrusage(RUSAGE_SELF, &r0);
    double resume = sweep(C, cold) + sweep(Wm, warm);
    struct rusage r1; getrusage(RUSAGE_SELF, &r1);
    qsort(hs, nh, sizeof(double), cmp); qsort(ws, nw, sizeof(double), cmp);
    FILE *f = fopen(argv[4], "w");
    fprintf(f, "mb=%zu fill_ms=%.0f hot_p50_ms=%.2f hot_p99_ms=%.2f hot_max_ms=%.2f warm_p50_ms=%.1f warm_max_ms=%.1f "
               "resume_ms=%.1f majflt_run=%ld majflt_resume=%ld\n",
            mb, fill_ms, q(hs, nh, .5), q(hs, nh, .99), q(hs, nh, 1), q(ws, nw, .5), q(ws, nw, 1),
            resume, r0.ru_majflt, r1.ru_majflt - r0.ru_majflt);
    fclose(f);
    return 0;
}
