// Many threads blocked at once: more than a Mac process may have host
// threads (kern.num_taskthreads, 12288), half in `read` on pipes, half in
// futex waits, then woken in turn (docs/processes.md, step 3a).
//
// Run as `blockmany N`. Starts N threads (small stacks); once all are
// blocked, prints "blocked N" and waits for a byte on stdin (the test
// counts the host's threads meanwhile); then wakes the readers one byte at
// a time and the futex waiters one at a time, joins them all, and prints
// "blockmany ok" and the time the wakes took.
#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { dprintf(2, "FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); _exit(1); } } while (0)
// Guest fds are few (Linux's default 1024): readers share pipes.
#define PIPES 256

static int pipes[PIPES][2];
static _Atomic uint32_t *words;
static atomic_int arrived, done_readers, done_waiters;

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static void *reader(void *arg) {
    int i = (int)(intptr_t)arg;
    atomic_fetch_add(&arrived, 1);
    char c;
    CHECK(read(pipes[i % PIPES][0], &c, 1) == 1);
    atomic_fetch_add(&done_readers, 1);
    return 0;
}

static void *waiter(void *arg) {
    int i = (int)(intptr_t)arg;
    atomic_fetch_add(&arrived, 1);
    while (atomic_load(&words[i]) == 0)
        syscall(SYS_futex, &words[i], FUTEX_WAIT_PRIVATE, 0, 0, 0, 0);
    atomic_fetch_add(&done_waiters, 1);
    return 0;
}

int main(int argc, char **argv) {
    int n = argc > 1 ? atoi(argv[1]) : 20000;
    int readers = n / 2, waiters = n - readers;
    for (int i = 0; i < PIPES; i++) CHECK(pipe(pipes[i]) == 0);
    words = calloc(waiters, sizeof *words);
    pthread_t *t = calloc(n, sizeof *t);
    CHECK(words && t);
    pthread_attr_t attr;
    pthread_attr_init(&attr);
    pthread_attr_setstacksize(&attr, 64 << 10);
    for (int i = 0; i < n; i++) {
        int r = i % 2 == 0 && i / 2 < readers;
        int k = i / 2;
        CHECK(pthread_create(&t[i], &attr, r ? reader : waiter, (void *)(intptr_t)k) == 0);
    }
    while (atomic_load(&arrived) < n) usleep(1000);
    // Past `arrived`, each is on its way into its wait: give them time.
    sleep(1);
    printf("blocked %d\n", n);
    fflush(stdout);
    char go;
    CHECK(read(0, &go, 1) == 1);
    double t0 = now_s();
    for (int k = 0; k < readers; k++) CHECK(write(pipes[k % PIPES][1], "x", 1) == 1);
    for (int k = 0; k < waiters; k++) {
        atomic_store(&words[k], 1);
        syscall(SYS_futex, &words[k], FUTEX_WAKE_PRIVATE, 1, 0, 0, 0);
    }
    for (int i = 0; i < n; i++) CHECK(pthread_join(t[i], 0) == 0);
    double took = now_s() - t0;
    CHECK(atomic_load(&done_readers) == readers && atomic_load(&done_waiters) == waiters);
    printf("blockmany ok\n");
    fprintf(stderr, "blockmany: %d threads woken and joined in %.2f s\n", n, took);
    return 0;
}
