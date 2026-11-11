// fakeserver — synthetic long-running "service" for server memory-offload benchmarks.
//
//   fakeserver <total_mb> <hot_pct> <warm_pct> <req_per_s> <hot_pages_per_req> <out_file>
//
// Allocates <total_mb> of anonymous memory filled with text-like data (compresses roughly like
// real heap memory) and splits it into three sets:
//   hot  — touched by every request; which pages is Zipf-like (log-uniform rank, scattered over
//          the set by a multiplicative hash), so a few pages are touched constantly and the tail
//          only every few tens of seconds — not a sequential sweep;
//   warm — 1 request in WARM_EVERY also touches WARM_PAGES uniformly random warm pages
//          (each warm page every few minutes: reports, rarely used indexes);
//   cold — never touched while serving (a cache that only matters after a failover).
// A client thread inside the process issues requests at a fixed rate (open loop: request i is
// due at t0 + i/rate, and its latency is measured from that due time, so a stall delays every
// request queued behind it — no coordinated omission). A request reads <hot_pages_per_req>
// pages (writes 1 in 8, so pages are dirtied like a real heap) and does a little work on each.
//
// Signals:
//   SIGUSR2  closes the current measurement segment (seg0 = run, seg1 = deploy, ...).
//   SIGUSR1  stops serving, writes the stats, then reads the whole cold set once ("cache warm-up
//            after failover"), times it, appends that, and exits.
// <out_file>.ready is written once the memory is filled (fill_ms = time to fault it all in).
//
// Output (<out_file>, key=value tokens, one line per record):
//   mb=… hot_pct=… warm_pct=… rate=… k=… fill_ms=… requests=… late=… majflt_serve=…
//   seg=0 requests=… p50_ms=… p99_ms=… p999_ms=… max_ms=…        (one per segment)
//   seg=all requests=… p50_ms=… p99_ms=… p999_ms=… max_ms=…
//   failover cold_mb=… cold_read_ms=… majflt_failover=…
// late = requests that started more than 100 ms after their due time.
//
// Build: gcc -O2 -static -pthread -o fakeserver fakeserver.c -lm
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

#define PG 4096UL
#define WARM_EVERY 20
#define WARM_PAGES 8
#define MAX_SEGS 16

static volatile sig_atomic_t stop_flag = 0, seg_flag = 0;
static void on_usr1(int s) { (void)s; stop_flag = 1; }
static void on_usr2(int s) { (void)s; seg_flag = 1; }

static double now_ms(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}
static inline uint64_t xr(uint64_t *s) { *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; return *s; }
static inline double unif(uint64_t *s) { return (xr(s) >> 11) * (1.0 / 9007199254740992.0); }

static const char *W[] = {"the","memory","page","function","return","value","object","string","array",
  "select","insert","update","index","table","row","column","session","token","cache","buffer","request",
  "response","json","query","commit","handler","promise","callback","tuple","node"};

static void fill(char *p, size_t n, uint64_t *rs) {
    size_t i = 0;
    while (i < n) {
        const char *w = W[xr(rs) % 30];
        size_t l = strlen(w);
        if (xr(rs) % 4 == 0) { uint64_t r = xr(rs); for (int k = 0; k < 6 && i < n; k++) p[i++] = 33 + (r >> (k * 8)) % 90; }
        for (size_t k = 0; k < l && i < n; k++) p[i++] = w[k];
        if (i < n) p[i++] = ' ';
    }
}

// ── shared state ──
static char *H, *Wm, *C;
static size_t hot, warm, cold;
static double rate;
static int kpp;
static volatile int serving = 1;
static float *lat;            // latency of every request, ms
static size_t nlat, caplat;
static size_t seg_start[MAX_SEGS + 1];
static int nseg = 0;
static size_t late;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static volatile uint64_t sink;

// Zipf-like rank in [0, n): P(rank < k) = log(k+1)/log(n+1), i.e. density ~ 1/rank.
static inline size_t zipf(uint64_t *s, size_t n) {
    double r = exp(unif(s) * log((double)n + 1.0)) - 1.0;
    size_t k = (size_t)r;
    return k >= n ? n - 1 : k;
}
// Scatter ranks over the set so hot pages are not one contiguous block.
static inline size_t scatter(size_t rank, size_t n) { return (size_t)(((unsigned __int128)rank * 0x9E3779B97F4A7C15ULL) % n); }

static uint64_t touch(char *page, int write, uint64_t salt) {
    volatile uint64_t *q = (volatile uint64_t *)page;
    uint64_t s = 0;
    for (int i = 0; i < 64; i += 8) s += q[i];  // a few cache lines of "work"
    if (write) q[(salt % 64)] = s ^ salt;
    return s;
}

static void *client(void *arg) {
    (void)arg;
    uint64_t rs = 0x9E3779B97F4A7C15ULL ^ (uint64_t)getpid() * 2654435761ULL;
    double t0 = now_ms(), period = 1000.0 / rate;
    uint64_t i = 0;
    while (serving) {
        double due = t0 + i * period, t = now_ms();
        if (t < due) {
            double w = due - t;
            struct timespec ts = {(time_t)(w / 1000), (long)(fmod(w, 1000.0) * 1e6)};
            nanosleep(&ts, NULL);
            continue;
        }
        if (t - due > 100.0) late++;
        uint64_t s = 0;
        for (int k = 0; k < kpp; k++) {
            size_t pg = scatter(zipf(&rs, hot), hot);
            s += touch(H + pg * PG, (xr(&rs) & 7) == 0, xr(&rs));
        }
        if (warm && i % WARM_EVERY == 0)
            for (int k = 0; k < WARM_PAGES; k++) s += touch(Wm + (xr(&rs) % warm) * PG, 0, 0);
        sink += s;
        float d = (float)(now_ms() - due);
        pthread_mutex_lock(&mu);
        if (nlat == caplat) {
            size_t nc = caplat ? caplat * 2 : (1 << 16);
            float *n = realloc(lat, nc * sizeof(float));
            if (n) { lat = n; caplat = nc; }
        }
        if (nlat < caplat) lat[nlat++] = d;
        pthread_mutex_unlock(&mu);
        i++;
    }
    return NULL;
}

static int cmpf(const void *a, const void *b) { float x = *(const float *)a, y = *(const float *)b; return x < y ? -1 : x > y; }
static double q(const float *v, size_t n, double p) { if (!n) return 0; return v[(size_t)((n - 1) * p)]; }

static void stats(FILE *f, const char *seg, const float *src, size_t n) {
    float *v = malloc((n ? n : 1) * sizeof(float));
    memcpy(v, src, n * sizeof(float));
    qsort(v, n, sizeof(float), cmpf);
    fprintf(f, "seg=%s requests=%zu p50_ms=%.3f p99_ms=%.3f p999_ms=%.3f max_ms=%.3f\n",
            seg, n, q(v, n, .5), q(v, n, .99), q(v, n, .999), n ? v[n - 1] : 0.0);
    free(v);
}

int main(int argc, char **argv) {
    if (argc < 7) { fprintf(stderr, "usage: fakeserver MB HOT%% WARM%% REQ_PER_S PAGES_PER_REQ OUT\n"); return 1; }
    size_t mb = atol(argv[1]); int hp = atoi(argv[2]), wp = atoi(argv[3]);
    rate = atof(argv[4]); kpp = atoi(argv[5]);
    const char *out = argv[6];
    if (mb == 0 || hp <= 0 || wp < 0 || hp + wp > 100 || rate <= 0 || kpp <= 0) { fprintf(stderr, "bad arguments\n"); return 1; }
    size_t pages = mb * 256;
    hot = pages * hp / 100; warm = pages * wp / 100; cold = pages - hot - warm;
    if (hot == 0) hot = 1;
    uint64_t rs = 88172645463325252ULL ^ (uint64_t)getpid() * 2654435761ULL;
    char *m = mmap(NULL, pages * PG, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (m == MAP_FAILED) { perror("mmap"); return 1; }
    madvise(m, pages * PG, MADV_NOHUGEPAGE);  // 4K pages, like most of a real heap
    double tf = now_ms();
    fill(m, pages * PG, &rs);
    double fill_ms = now_ms() - tf;
    H = m; Wm = m + hot * PG; C = m + (hot + warm) * PG;

    signal(SIGUSR1, on_usr1);
    signal(SIGUSR2, on_usr2);
    struct rusage r0; getrusage(RUSAGE_SELF, &r0);
    pthread_t th;
    if (pthread_create(&th, NULL, client, NULL)) { perror("pthread_create"); return 1; }
    { char rp[512]; snprintf(rp, sizeof rp, "%s.ready", out); FILE *rf = fopen(rp, "w"); if (rf) { fprintf(rf, "fill_ms=%.0f\n", fill_ms); fclose(rf); } }

    seg_start[0] = 0;
    while (!stop_flag) {
        if (seg_flag) {
            seg_flag = 0;
            pthread_mutex_lock(&mu);
            if (nseg < MAX_SEGS - 1) seg_start[++nseg] = nlat;
            pthread_mutex_unlock(&mu);
        }
        usleep(100000);
    }
    serving = 0;
    pthread_join(th, NULL);
    struct rusage r1; getrusage(RUSAGE_SELF, &r1);
    seg_start[nseg + 1] = nlat;

    FILE *f = fopen(out, "w");
    if (!f) { perror(out); return 1; }
    fprintf(f, "mb=%zu hot_pct=%d warm_pct=%d rate=%.0f k=%d fill_ms=%.0f requests=%zu late=%zu majflt_serve=%ld\n",
            mb, hp, wp, rate, kpp, fill_ms, nlat, late, r1.ru_majflt - r0.ru_majflt);
    for (int s = 0; s <= nseg; s++) {
        char name[16]; snprintf(name, sizeof name, "%d", s);
        stats(f, name, lat + seg_start[s], seg_start[s + 1] - seg_start[s]);
    }
    stats(f, "all", lat, nlat);
    fflush(f);

    // Failover: a standby takes over and warms its cache — every cold page read once.
    double tc = now_ms(); uint64_t s = 0;
    for (size_t i = 0; i < cold; i++) s += (unsigned char)C[i * PG + (i % 64) * 8];
    sink += s;
    double cold_ms = now_ms() - tc;
    struct rusage r2; getrusage(RUSAGE_SELF, &r2);
    fprintf(f, "failover cold_mb=%zu cold_read_ms=%.1f majflt_failover=%ld\n", cold / 256, cold_ms, r2.ru_majflt - r1.ru_majflt);
    fclose(f);
    return 0;
}
