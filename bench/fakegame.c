// fakegame — a synthetic game for the desktop "game" scenario.
//
//   fakegame <base_mb> <level_mb> <level_at_s> <threads> <out_file>
//
// Loads <base_mb> of assets, then renders frames at 60 fps on <threads> threads. Every frame each
// thread does a fixed amount of work (calibrated at start to ~70% of a 16.7 ms frame, so the game
// keeps the CPUs busy like a real one) and reads a slice of the game's memory, so the whole working
// set is touched every couple of seconds: none of it is cold. At <level_at_s> it loads the next
// level: <level_mb> more memory, timed, and from then on part of the working set too.
//
// A frame is "late" when it takes longer than 2 × 16.7 ms. SIGUSR1 stops the game and writes:
//   frames=… p50_ms=… p99_ms=… p999_ms=… max_ms=… late=… level_ms=… avail_before_level_mb=…
//   majflt=…
#define _GNU_SOURCE
#include <math.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <time.h>
#include <unistd.h>

#define PG 4096
#define FRAME_MS (1000.0 / 60.0)
#define MAX_FRAMES (60 * 3600)
#define MAX_THREADS 16

static volatile int stop;
static void on_usr1(int s) { (void)s; stop = 1; }

static double now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}
static inline uint64_t xs(uint64_t *s) { *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; return *s; }

static char *mem;               // base + level, contiguous
static volatile size_t pages;   // pages currently in the working set
static uint64_t work_iters;     // compute per frame per thread
static int nthreads;
static float *ft[MAX_THREADS];  // frame times per thread
static size_t nf[MAX_THREADS];
static volatile uint64_t sink;

static uint64_t compute(uint64_t n, uint64_t seed) {
    double a = (double)(seed & 0xffff) + 1.0;
    for (uint64_t i = 0; i < n; i++) a = a * 1.0000001 + sqrt(a) * 1e-9;
    return (uint64_t)a;
}

static void fill(char *p, size_t n, uint64_t seed) {
    // Game assets: textures and meshes, which compress far less than text.
    uint64_t s = seed | 1;
    for (size_t i = 0; i + 8 <= n; i += 8) {
        uint64_t v = xs(&s);
        if ((i >> 12) & 1) v &= 0x0f0f0f0f0f0f0f0fULL; // half the pages somewhat compressible
        memcpy(p + i, &v, 8);
    }
}

static void *worker(void *arg) {
    int id = (int)(intptr_t)arg;
    uint64_t s = 0x9E3779B97F4A7C15ULL * (id + 1);
    double next = now_ms();
    while (!stop) {
        double t0 = now_ms();
        uint64_t acc = compute(work_iters, xs(&s));
        // Each thread reads 1/(threads * 120) of the working set per frame: all of it every ~2 s.
        size_t np = pages, per = np / (size_t)(nthreads * 120) + 1;
        for (size_t k = 0; k < per; k++) acc += mem[(xs(&s) % np) * PG + (k & 63)];
        sink += acc;
        double dt = now_ms() - t0;
        if (nf[id] < MAX_FRAMES) ft[id][nf[id]++] = (float)dt;
        next += FRAME_MS;
        double w = next - now_ms();
        if (w > 0) usleep((useconds_t)(w * 1000));
        else next = now_ms(); // fell behind: do not try to catch up
    }
    return NULL;
}

static int cmpf(const void *a, const void *b) { float x = *(const float *)a, y = *(const float *)b; return (x > y) - (x < y); }

static long avail_mb(void) {
    FILE *f = fopen("/proc/meminfo", "r");
    char l[256];
    long kb = -1;
    while (f && fgets(l, sizeof l, f))
        if (sscanf(l, "MemAvailable: %ld kB", &kb) == 1) break;
    if (f) fclose(f);
    return kb / 1024;
}

int main(int argc, char **argv) {
    if (argc != 6) {
        fprintf(stderr, "usage: fakegame <base_mb> <level_mb> <level_at_s> <threads> <out_file>\n");
        return 2;
    }
    size_t base = strtoull(argv[1], 0, 10) * 1024 * 1024 / PG, level = strtoull(argv[2], 0, 10) * 1024 * 1024 / PG;
    double level_at = atof(argv[3]) * 1000;
    nthreads = atoi(argv[4]);
    if (nthreads < 1 || nthreads > MAX_THREADS) return 2;
    const char *out = argv[5];
    signal(SIGUSR1, on_usr1);

    mem = mmap(0, (base + level) * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mem == MAP_FAILED) return 1;
    double t = now_ms();
    fill(mem, base * PG, 1);
    double load_ms = now_ms() - t;
    pages = base;

    // Calibrate: work_iters ≈ 70% of a frame on this machine.
    work_iters = 100000;
    for (;;) {
        double a = now_ms();
        sink += compute(work_iters, 7);
        double d = now_ms() - a;
        if (d > 2.0) { work_iters = (uint64_t)(work_iters * (0.7 * FRAME_MS) / d); break; }
        work_iters *= 4;
    }
    { char rp[512]; snprintf(rp, sizeof rp, "%s.ready", out); FILE *rf = fopen(rp, "w"); if (rf) { fprintf(rf, "load_ms=%.0f\n", load_ms); fclose(rf); } }

    for (int i = 0; i < nthreads; i++) ft[i] = malloc(sizeof(float) * MAX_FRAMES);
    pthread_t th[MAX_THREADS];
    for (int i = 0; i < nthreads; i++) pthread_create(&th[i], 0, worker, (void *)(intptr_t)i);

    struct rusage r0;
    getrusage(RUSAGE_SELF, &r0);
    double start = now_ms(), level_ms = -1;
    long avail_before = -1;
    while (!stop) {
        if (level_ms < 0 && level && now_ms() - start >= level_at) {
            avail_before = avail_mb();
            double a = now_ms();
            fill(mem + base * PG, level * PG, 2);
            level_ms = now_ms() - a;
            pages = base + level;
        }
        usleep(100000);
    }
    for (int i = 0; i < nthreads; i++) pthread_join(th[i], 0);
    struct rusage r1;
    getrusage(RUSAGE_SELF, &r1);

    size_t n = 0;
    for (int i = 0; i < nthreads; i++) n += nf[i];
    float *all = malloc(sizeof(float) * (n + 1));
    size_t k = 0, late = 0;
    for (int i = 0; i < nthreads; i++)
        for (size_t j = 0; j < nf[i]; j++) { all[k++] = ft[i][j]; if (ft[i][j] > 2 * FRAME_MS) late++; }
    qsort(all, n, sizeof(float), cmpf);
#define Q(p) (n ? all[(size_t)((n - 1) * (p))] : 0)
    FILE *f = fopen(out, "w");
    if (!f) return 1;
    fprintf(f, "frames=%zu p50_ms=%.2f p99_ms=%.2f p999_ms=%.2f max_ms=%.2f late=%zu level_ms=%.0f avail_before_level_mb=%ld majflt=%ld load_ms=%.0f\n",
            n, Q(.5), Q(.99), Q(.999), n ? all[n - 1] : 0, late, level_ms, avail_before, r1.ru_majflt - r0.ru_majflt, load_ms);
    fclose(f);
    return 0;
}
