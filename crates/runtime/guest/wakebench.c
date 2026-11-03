// Wakeup latency: two parties hand a token back and forth, each blocked in
// the call that waits for it, and one of them times every round trip.
//
// Run as `wakebench MODE N [SPIN]`. MODE is how the token goes:
//   pipe-proc     a byte over two pipes, between two processes
//   pipe-thread   the same, between two threads
//   unix-proc     a byte over a Unix socket pair, between two processes
//   unix-thread   the same, between two threads
//   eventfd       a count over two eventfds, between two threads
//   futex         a word and FUTEX_WAIT/FUTEX_WAKE, between two threads
//   futex-proc    the same between two processes, the word in shared
//                 memory (non-private futex ops)
//   mutex-proc    a process-shared mutex in shared memory handed over: the
//                 time from the holder's unlock to the other process,
//                 blocked in pthread_mutex_lock, holding it
//   sleep         no partner: nanosleep(50 us), the time it took
// N is the number of rounds timed (after 200 not timed). SPIN threads (0 by
// default) spin meanwhile, so that blocked calls give their CPU up (with
// MACWL_CPUS small, each wait then leaves the CPU instead of blocking on it).
// Prints "MODE p50 p99 mean" in microseconds.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { dprintf(2, "FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); _exit(1); } } while (0)
#define WARMUP 200

static double now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e6 + ts.tv_nsec / 1e3;
}

static int rounds;
// The two directions: ping writes to[0], reads from[0]... as fds for pipes
// and sockets: there, ping uses a[1]/b[0], pong b[1]/a[0].
static int a[2], b[2];
static int efd[2];
static _Atomic uint32_t word_here;
static _Atomic uint32_t *wordp = &word_here;
static int futex_flags = FUTEX_PRIVATE_FLAG;
static atomic_int stop_spinning;

static void put(int fd) {
    char c = 1;
    CHECK(write(fd, &c, 1) == 1);
}

static void get(int fd) {
    char c;
    CHECK(read(fd, &c, 1) == 1);
}

static void futex_wait(uint32_t val) {
    while (atomic_load(wordp) == val)
        syscall(SYS_futex, wordp, FUTEX_WAIT | futex_flags, val, 0, 0, 0);
}

static void futex_set(uint32_t val) {
    atomic_store(wordp, val);
    syscall(SYS_futex, wordp, FUTEX_WAKE | futex_flags, 1, 0, 0, 0);
}

// mutex-proc: in shared memory.
struct handoff {
    pthread_mutex_t m;
    atomic_int phase;
    _Atomic double t0, took;
};
static struct handoff *ho;

static void until_phase(int p) {
    while (atomic_load(&ho->phase) != p)
        sched_yield();
}

static const char *mode;

// The partner: answers each token.
static void pong(void) {
    int total = rounds + WARMUP;
    if (!strcmp(mode, "eventfd")) {
        for (int i = 0; i < total; i++) {
            uint64_t v;
            CHECK(read(efd[0], &v, 8) == 8);
            v = 1;
            CHECK(write(efd[1], &v, 8) == 8);
        }
    } else if (!strcmp(mode, "mutex-proc")) {
        for (int i = 0; i < total; i++) {
            until_phase(1);
            atomic_store(&ho->phase, 2);
            CHECK(pthread_mutex_lock(&ho->m) == 0);
            atomic_store(&ho->took, now_us() - atomic_load(&ho->t0));
            CHECK(pthread_mutex_unlock(&ho->m) == 0);
            atomic_store(&ho->phase, 3);
        }
    } else if (!strncmp(mode, "futex", 5)) {
        for (int i = 0; i < total; i++) {
            futex_wait(0); // until ping makes it 1
            futex_set(0 + 2); // 2: answered
            futex_wait(2); // until ping takes it back to 0
        }
    } else {
        for (int i = 0; i < total; i++) {
            get(a[0]);
            put(b[1]);
        }
    }
}

static void *pong_thread(void *arg) {
    (void)arg;
    pong();
    return 0;
}

static void *spinner(void *arg) {
    (void)arg;
    static volatile unsigned long n;
    while (!atomic_load(&stop_spinning)) n = n + 1;
    return 0;
}

static int cmp(const void *x, const void *y) {
    double d = *(const double *)x - *(const double *)y;
    return d < 0 ? -1 : d > 0;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        dprintf(2, "usage: wakebench MODE N [SPIN]\n");
        return 2;
    }
    mode = argv[1];
    rounds = atoi(argv[2]);
    int spin = argc > 3 ? atoi(argv[3]) : 0;
    double *t = calloc(rounds, sizeof *t);
    CHECK(t);
    pthread_t spinners[64];
    for (int i = 0; i < spin && i < 64; i++) CHECK(pthread_create(&spinners[i], 0, spinner, 0) == 0);

    int procs = !strcmp(mode, "pipe-proc") || !strcmp(mode, "unix-proc") || !strcmp(mode, "futex-proc") || !strcmp(mode, "mutex-proc");
    if (!strncmp(mode, "pipe", 4)) {
        CHECK(pipe(a) == 0 && pipe(b) == 0);
    } else if (!strncmp(mode, "unix", 4)) {
        // One socket pair carries both directions: a[0] and b[1] are the
        // same end, a[1] and b[0] the other.
        int sv[2];
        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
        a[0] = sv[0], b[1] = sv[0], a[1] = sv[1], b[0] = sv[1];
    } else if (!strcmp(mode, "eventfd")) {
        // ping writes efd A and reads B; pong reads A and writes B.
        int x = eventfd(0, 0), y = eventfd(0, 0);
        CHECK(x >= 0 && y >= 0);
        efd[0] = x, efd[1] = y;
    } else if (!strcmp(mode, "sleep")) {
        for (int i = 0; i < rounds + WARMUP; i++) {
            double t0 = now_us();
            struct timespec ts = { 0, 50000 };
            nanosleep(&ts, 0);
            if (i >= WARMUP) t[i - WARMUP] = now_us() - t0;
        }
        goto report;
    } else if (!strcmp(mode, "futex-proc")) {
        wordp = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        CHECK(wordp != MAP_FAILED);
        futex_flags = 0;
    } else if (!strcmp(mode, "mutex-proc")) {
        ho = mmap(0, sizeof *ho, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        CHECK(ho != MAP_FAILED);
        pthread_mutexattr_t ma;
        pthread_mutexattr_init(&ma);
        CHECK(pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED) == 0);
        CHECK(pthread_mutex_init(&ho->m, &ma) == 0);
    } else if (strcmp(mode, "futex")) {
        dprintf(2, "wakebench: unknown mode %s\n", mode);
        return 2;
    }

    pid_t child = -1;
    pthread_t partner;
    if (procs) {
        child = fork();
        CHECK(child >= 0);
        if (child == 0) {
            pong();
            _exit(0);
        }
    } else {
        CHECK(pthread_create(&partner, 0, pong_thread, 0) == 0);
    }

    for (int i = 0; i < rounds + WARMUP; i++) {
        double t0 = now_us();
        if (!strcmp(mode, "eventfd")) {
            uint64_t v = 1;
            CHECK(write(efd[0], &v, 8) == 8);
            CHECK(read(efd[1], &v, 8) == 8);
        } else if (!strcmp(mode, "mutex-proc")) {
            // The other process blocks in pthread_mutex_lock (given 200 us
            // to get there) while this one holds the mutex; then it's let go.
            CHECK(pthread_mutex_lock(&ho->m) == 0);
            atomic_store(&ho->phase, 1);
            until_phase(2);
            struct timespec ts = { 0, 200000 };
            nanosleep(&ts, 0);
            atomic_store(&ho->t0, now_us());
            CHECK(pthread_mutex_unlock(&ho->m) == 0);
            until_phase(3);
            atomic_store(&ho->phase, 0);
            if (i >= WARMUP) t[i - WARMUP] = atomic_load(&ho->took);
            continue;
        } else if (!strncmp(mode, "futex", 5)) {
            futex_set(1);
            futex_wait(1); // until pong makes it 2
            futex_set(0);
        } else {
            put(a[1]);
            get(b[0]);
        }
        if (i >= WARMUP) t[i - WARMUP] = now_us() - t0;
    }
    if (procs) {
        int st;
        CHECK(waitpid(child, &st, 0) == child && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    } else {
        CHECK(pthread_join(partner, 0) == 0);
    }

report:
    atomic_store(&stop_spinning, 1);
    for (int i = 0; i < spin && i < 64; i++) pthread_join(spinners[i], 0);
    double sum = 0;
    for (int i = 0; i < rounds; i++) sum += t[i];
    qsort(t, rounds, sizeof *t, cmp);
    printf("%s %.1f %.1f %.1f\n", mode, t[rounds / 2], t[rounds * 99 / 100], sum / rounds);
    return 0;
}
