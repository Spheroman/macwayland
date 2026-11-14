// Guest threads and timers past the host's thread budget
// (linux_abi::host_threads): run with MACWL_MAX_HOST_THREADS=32. None of
// this may cost a host thread each, or the host goes over its ceiling (48)
// and its watchdog ends it.
//
// - `flock`: 100 threads wait for a lock the main thread holds. Each waits
//   on a worker, and workers are bounded by the budget, so most wait for a
//   worker; they all get the lock in turn once it's let go.
// - Timers: 200 POSIX timers armed at once (with a thread each, as before,
//   that alone would be 200 host threads), each firing a few times.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { dprintf(2, "FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); _exit(1); } } while (0)

#define LOCKERS 100
#define TIMERS 200

static char path[64];
static atomic_int locked, waiting;

static void *locker(void *arg) {
    (void)arg;
    int fd = open(path, O_RDWR);
    CHECK(fd >= 0);
    atomic_fetch_add(&waiting, 1);
    CHECK(flock(fd, LOCK_EX) == 0);
    atomic_fetch_add(&locked, 1);
    CHECK(flock(fd, LOCK_UN) == 0);
    close(fd);
    return NULL;
}

static atomic_int fired[TIMERS];

static void on_timer(int sig, siginfo_t *si, void *uc) {
    (void)sig, (void)uc;
    // Expiries while its last signal was pending come as the overrun.
    int i = si->si_value.sival_int;
    if (i >= 0 && i < TIMERS) atomic_fetch_add(&fired[i], 1 + si->si_overrun);
}

static void sleep_ms(int ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {}
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    // `hostbudget timers`: the timers alone.
    int flocks = !(argc > 1 && !strcmp(argv[1], "timers"));
    // flock
    if (flocks) {
    snprintf(path, sizeof path, "/tmp/hostbudget.%d", getpid());
    int fd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    CHECK(fd >= 0);
    CHECK(flock(fd, LOCK_EX) == 0);
    pthread_t t[LOCKERS];
    for (int i = 0; i < LOCKERS; i++) CHECK(pthread_create(&t[i], NULL, locker, NULL) == 0);
    for (int i = 0; i < 500 && atomic_load(&waiting) < LOCKERS; i++) sleep_ms(2);
    CHECK(atomic_load(&waiting) == LOCKERS);
    sleep_ms(100); // let them block, as far as workers allow
    CHECK(atomic_load(&locked) == 0);
    CHECK(flock(fd, LOCK_UN) == 0);
    for (int i = 0; i < LOCKERS; i++) CHECK(pthread_join(t[i], NULL) == 0);
    CHECK(atomic_load(&locked) == LOCKERS);
    close(fd);
    unlink(path);
    printf("flock: %d threads got the lock in turn\n", LOCKERS);
    }

    // Timers
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = on_timer;
    sa.sa_flags = SA_SIGINFO | SA_RESTART;
    CHECK(sigaction(SIGRTMIN, &sa, NULL) == 0);
    timer_t ids[TIMERS];
    for (int i = 0; i < TIMERS; i++) {
        struct sigevent ev;
        memset(&ev, 0, sizeof ev);
        ev.sigev_notify = SIGEV_SIGNAL;
        ev.sigev_signo = SIGRTMIN;
        ev.sigev_value.sival_int = i;
        CHECK(timer_create(CLOCK_MONOTONIC, &ev, &ids[i]) == 0);
        // Staggered starts, every 20 ms after.
        struct itimerspec its = { { 0, 20000000 }, { 0, 1000000 + i * 50000 } };
        CHECK(timer_settime(ids[i], 0, &its, NULL) == 0);
    }
    sleep_ms(300);
    for (int i = 0; i < TIMERS; i++) {
        // musl's timer_delete returns the call's result itself, not -1 and
        // errno.
        int r = timer_delete(ids[i]);
        if (r != 0) { dprintf(2, "FAIL: timer_delete(%d) of timer %d returned %d\n", (int)(intptr_t)ids[i], i, r); _exit(1); }
    }
    int least = 1 << 30, most = 0;
    for (int i = 0; i < TIMERS; i++) {
        int n = atomic_load(&fired[i]);
        if (n < least) least = n;
        if (n > most) most = n;
    }
    printf("timers: fired %d to %d times each\n", least, most);
    CHECK(least >= 10); // 300 ms at 20 ms, with starts up to 10 ms in: ~15
    printf("timers: %d armed at once, each fired at least %d times\n", TIMERS, least);
    printf("hostbudget ok\n");
    return 0;
}
