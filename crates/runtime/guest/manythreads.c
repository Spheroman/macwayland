// More threads than CPUs: see the `manythreads` test in crates/runtime/tests/run.rs
// (run with the default CPUs, and with MACWL_CPUS=2 and 1). Threads that
// block give up their CPU (docs/mn-threads.md, steps 3 and 4).
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <linux/futex.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "failed: %s (line %d, errno %d)\n", #c, __LINE__, errno); exit(1); } } while (0)

// Fixed work, then done: more of them than there are CPUs.
static atomic_int finished;
static void *work(void *arg) {
    volatile unsigned long x = (unsigned long)arg;
    for (int i = 0; i < 2000000; i++)
        x = x * 6364136223846793005UL + 1;
    atomic_fetch_add(&finished, 1);
    return NULL;
}

// Spinners: each counts until told to stop. Sharing CPUs, every one of them
// must get some time (preemption).
#define SPINNERS 6
static atomic_int stop;
static atomic_ulong counts[SPINNERS];
static void *spin(void *arg) {
    long i = (long)arg;
    while (!atomic_load(&stop))
        atomic_fetch_add(&counts[i], 1);
    return NULL;
}

// A thread that gets a signal, maybe while waiting for a CPU.
static atomic_int got_usr1;
static atomic_int usr1_tid;
static void on_usr1(int sig) {
    (void)sig;
    atomic_store(&usr1_tid, (int)syscall(SYS_gettid));
    atomic_store(&got_usr1, 1);
}
static atomic_int target_tid;
static void *target(void *arg) {
    (void)arg;
    atomic_store(&target_tid, (int)syscall(SYS_gettid));
    while (!atomic_load(&got_usr1))
        ;
    return NULL;
}

// Spins for ever: what execve and exit_group must end, running or waiting
// for a CPU.
static void *forever(void *arg) {
    (void)arg;
    for (;;)
        atomic_fetch_add(&counts[0], 1);
    return NULL;
}

// 200 threads waiting on one condition variable, woken one at a time, then
// all at once.
enum { WAITERS = 200 };
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int tickets, go_all, waiting;
static void *waiter(void *arg) {
    long all = (long)arg;
    pthread_mutex_lock(&mu);
    waiting++;
    if (all) {
        while (!go_all)
            pthread_cond_wait(&cv, &mu);
    } else {
        while (tickets == 0)
            pthread_cond_wait(&cv, &mu);
        tickets--;
    }
    waiting--;
    pthread_mutex_unlock(&mu);
    return NULL;
}

// A token passed around a ring of 200 threads through pipes: each blocks in
// read until its turn.
enum { RING = 200, LAPS = 3 };
static int ring[RING][2];
static void *link_(void *arg) {
    long i = (long)arg;
    char c;
    for (int lap = 0; lap < LAPS; lap++) {
        CHECK(read(ring[i][0], &c, 1) == 1);
        CHECK(write(ring[(i + 1) % RING][1], &c, 1) == 1);
    }
    return NULL;
}

// A semaphore on a raw futex, waited on while spinners keep every CPU busy:
// a wait whose CPU is taken back wakes spuriously, and must not say EINTR
// for that, which Linux gives only for a signal (glibc's sem_wait passes it
// on; musl's, which these tests use, would hide it).
enum { SEM_WAITERS = 40, SEM_ROUNDS = 50 };
static atomic_int sem;
static void *sem_waiter(void *arg) {
    (void)arg;
    for (int i = 0; i < SEM_ROUNDS; i++) {
        for (;;) {
            int v = atomic_load(&sem);
            if (v > 0 && atomic_compare_exchange_weak(&sem, &v, v - 1))
                break;
            if (v <= 0 && syscall(SYS_futex, &sem, FUTEX_WAIT_PRIVATE, v, NULL) < 0)
                CHECK(errno == EAGAIN);
        }
    }
    return NULL;
}

static void sem_post_(void) {
    atomic_fetch_add(&sem, 1);
    syscall(SYS_futex, &sem, FUTEX_WAKE_PRIVATE, 1);
}

static void sleep_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000 };
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR)
        ;
}

int main(int argc, char **argv) {
    pthread_t f[8];
    if (argc > 1) {
        // After the execve: the threads before it are gone. Start more, and
        // end the process under them.
        for (int i = 0; i < 8; i++)
            CHECK(pthread_create(&f[i], NULL, forever, NULL) == 0);
        sleep_ms(50);
        printf("manythreads ok\n");
        fflush(stdout);
        exit(0);
    }

    // Each phase, as it starts, on stderr: where a hang was.
#define PHASE(name) fprintf(stderr, "phase %s\n", name)
    PHASE("work");
    // 150 threads at once: more than the 64 vCPUs a process can have.
    enum { N = 150 };
    pthread_t t[N];
    for (long i = 0; i < N; i++)
        CHECK(pthread_create(&t[i], NULL, work, (void *)i) == 0);
    for (int i = 0; i < N; i++)
        CHECK(pthread_join(t[i], NULL) == 0);
    CHECK(atomic_load(&finished) == N);

    PHASE("cond-one");
    // Condition variables: one at a time, then all at once.
    pthread_t w[WAITERS];
    for (long i = 0; i < WAITERS; i++)
        CHECK(pthread_create(&w[i], NULL, waiter, (void *)0) == 0);
    for (int i = 0; i < WAITERS; i++) {
        pthread_mutex_lock(&mu);
        tickets++;
        pthread_cond_signal(&cv);
        pthread_mutex_unlock(&mu);
    }
    for (int i = 0; i < WAITERS; i++)
        CHECK(pthread_join(w[i], NULL) == 0);
    PHASE("cond-all");
    for (long i = 0; i < WAITERS; i++)
        CHECK(pthread_create(&w[i], NULL, waiter, (void *)1) == 0);
    for (;;) {
        pthread_mutex_lock(&mu);
        int n = waiting;
        pthread_mutex_unlock(&mu);
        if (n == WAITERS)
            break;
        sleep_ms(1);
    }
    pthread_mutex_lock(&mu);
    go_all = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&mu);
    for (int i = 0; i < WAITERS; i++)
        CHECK(pthread_join(w[i], NULL) == 0);

    PHASE("ring");
    // The ring: every thread but one blocked in read at any moment.
    pthread_t r[RING];
    for (int i = 0; i < RING; i++)
        CHECK(pipe(ring[i]) == 0);
    for (long i = 0; i < RING; i++)
        CHECK(pthread_create(&r[i], NULL, link_, (void *)i) == 0);
    CHECK(write(ring[0][1], "t", 1) == 1);
    for (int i = 0; i < RING; i++)
        CHECK(pthread_join(r[i], NULL) == 0);
    char token;
    CHECK(read(ring[0][0], &token, 1) == 1 && token == 't');

    PHASE("spin");
    // Spinners share the CPUs while the main thread sleeps: each gets time.
    pthread_t s[SPINNERS];
    for (long i = 0; i < SPINNERS; i++)
        CHECK(pthread_create(&s[i], NULL, spin, (void *)i) == 0);
    sleep_ms(400);
    atomic_store(&stop, 1);
    for (int i = 0; i < SPINNERS; i++)
        CHECK(pthread_join(s[i], NULL) == 0);
    for (int i = 0; i < SPINNERS; i++) {
        if (atomic_load(&counts[i]) == 0) {
            fprintf(stderr, "spinner %d never ran\n", i);
            exit(1);
        }
    }

    PHASE("sem");
    atomic_store(&stop, 0);
    for (long i = 0; i < SPINNERS; i++)
        CHECK(pthread_create(&s[i], NULL, spin, (void *)i) == 0);
    pthread_t sw[SEM_WAITERS];
    for (long i = 0; i < SEM_WAITERS; i++)
        CHECK(pthread_create(&sw[i], NULL, sem_waiter, NULL) == 0);
    for (int i = 0; i < SEM_WAITERS * SEM_ROUNDS; i++) {
        sem_post_();
        if (i % 100 == 0)
            sleep_ms(1);
    }
    for (int i = 0; i < SEM_WAITERS; i++)
        CHECK(pthread_join(sw[i], NULL) == 0);
    atomic_store(&stop, 1);
    for (int i = 0; i < SPINNERS; i++)
        CHECK(pthread_join(s[i], NULL) == 0);

    PHASE("signal");
    // A signal to a thread, handled by that thread.
    struct sigaction sa = { .sa_handler = on_usr1 };
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(SIGUSR1, &sa, NULL) == 0);
    pthread_t tt;
    CHECK(pthread_create(&tt, NULL, target, NULL) == 0);
    while (!atomic_load(&target_tid))
        sleep_ms(1);
    CHECK(pthread_kill(tt, SIGUSR1) == 0);
    CHECK(pthread_join(tt, NULL) == 0);
    CHECK(atomic_load(&usr1_tid) == atomic_load(&target_tid));

    PHASE("exec");
    // execve with threads running and waiting for a CPU: they end, and the
    // new program (this one again) runs alone.
    for (int i = 0; i < 8; i++)
        CHECK(pthread_create(&f[i], NULL, forever, NULL) == 0);
    sleep_ms(50);
    char self[4096];
    ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
    CHECK(n > 0);
    self[n] = 0;
    char *args[] = { argv[0], "after-exec", NULL };
    execv(self, args);
    CHECK(!"execv");
    return 1;
}
