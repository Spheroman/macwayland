// Signal handlers: delivery and return, siginfo and ucontext, masks,
// SA_RESTART, calls that don't wait under a stream of signals, the
// alternate stack, faults, timers (a host signal arriving
// while the guest runs), signals from another process, and SIGPIPE.
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <setjmp.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s\n", __LINE__, #cond); fflush(stdout); _exit(1); } } while (0)

static volatile sig_atomic_t hits, last_sig, last_code, last_pid;
static volatile uintptr_t last_addr, handler_sp;
static sigjmp_buf jump;

static void on_signal(int sig, siginfo_t *info, void *uc) {
    (void)uc;
    hits++;
    last_sig = sig;
    last_code = info->si_code;
    last_pid = info->si_pid;
    int local;
    handler_sp = (uintptr_t)&local;
}

static void on_segv(int sig, siginfo_t *info, void *ucv) {
    ucontext_t *uc = ucv;
    last_sig = sig;
    last_addr = (uintptr_t)info->si_addr;
    CHECK(uc->uc_mcontext.fault_address == (uintptr_t)info->si_addr);
    CHECK(uc->uc_mcontext.pc != 0);
    siglongjmp(jump, 1);
}

static void install(int sig, void (*fn)(int, siginfo_t *, void *), int flags) {
    struct sigaction sa = {0};
    sa.sa_sigaction = fn;
    sa.sa_flags = SA_SIGINFO | flags;
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(sig, &sa, 0) == 0);
}

static void basics(void) {
    install(SIGUSR1, on_signal, 0);
    // Registers survive a handler running in between.
    volatile double d = 1.5;
    volatile long x = 42;
    CHECK(raise(SIGUSR1) == 0);
    CHECK(hits == 1 && last_sig == SIGUSR1 && last_pid == getpid());
    CHECK(d == 1.5 && x == 42);

    // Blocked, it waits; unblocked, it runs.
    sigset_t s, old;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigprocmask(SIG_BLOCK, &s, &old);
    raise(SIGUSR1);
    CHECK(hits == 1);
    sigset_t pend;
    sigpending(&pend);
    CHECK(sigismember(&pend, SIGUSR1));
    sigprocmask(SIG_SETMASK, &old, 0);
    CHECK(hits == 2);
}

static void alternate_stack(void) {
    static char stack[64 * 1024];
    stack_t ss = {.ss_sp = stack, .ss_size = sizeof stack, .ss_flags = 0};
    CHECK(sigaltstack(&ss, 0) == 0);
    install(SIGUSR2, on_signal, SA_ONSTACK);
    raise(SIGUSR2);
    CHECK(last_sig == SIGUSR2);
    CHECK(handler_sp > (uintptr_t)stack && handler_sp < (uintptr_t)stack + sizeof stack);
}

static void faults(void) {
    install(SIGSEGV, on_segv, 0);
    if (sigsetjmp(jump, 1) == 0) {
        int *volatile bad = (int *)0x8;
        *bad = 1;
        CHECK(!"survived a null write");
    }
    CHECK(last_sig == SIGSEGV && last_addr == 0x8);
    signal(SIGSEGV, SIG_DFL);
}

static void timer_interrupts_a_busy_loop(void) {
    install(SIGALRM, on_signal, 0);
    int before = hits;
    struct itimerval it = {.it_value = {.tv_usec = 20000}};
    CHECK(setitimer(ITIMER_REAL, &it, 0) == 0);
    // Spin without system calls: only a kick out of the guest can deliver it.
    for (volatile long i = 0; hits == before; i++) {
        CHECK(i < 4000000000L);
    }
    CHECK(last_sig == SIGALRM);
}

static void restart_and_eintr(void) {
    int p[2];
    CHECK(pipe(p) == 0);
    pid_t child = fork();
    if (child == 0) {
        usleep(100000);
        CHECK(write(p[1], "z", 1) == 1);
        _exit(0);
    }
    // SA_RESTART: the read carries on after the handler and gets the data.
    install(SIGALRM, on_signal, SA_RESTART);
    struct itimerval it = {.it_value = {.tv_usec = 20000}};
    setitimer(ITIMER_REAL, &it, 0);
    int before = hits;
    char c;
    CHECK(read(p[0], &c, 1) == 1 && c == 'z');
    CHECK(hits == before + 1);
    waitpid(child, 0, 0);
    // Without it, the read fails with EINTR.
    install(SIGALRM, on_signal, 0);
    setitimer(ITIMER_REAL, &it, 0);
    CHECK(read(p[0], &c, 1) == -1 && errno == EINTR);
    close(p[0]), close(p[1]);
}

// Calls that don't wait aren't interrupted: on Linux a signal ends only a
// wait, so a futex wake, a read or write that's ready, and a nonblocking
// read never fail with EINTR, however signals come (glibc aborts on a
// wake's EINTR: "The futex facility returned an unexpected error code").
// Another thread signals this one all along, to a handler without
// SA_RESTART.
static volatile int pelting;
static void *pelt(void *arg) {
    pthread_t target = *(pthread_t *)arg;
    while (pelting) {
        pthread_kill(target, SIGUSR2);
        for (volatile int i = 0; i < 20; i++) {}
    }
    return 0;
}

static void calls_that_dont_wait(void) {
    install(SIGUSR2, on_signal, 0);
    int p[2];
    CHECK(pipe(p) == 0);
    CHECK(fcntl(p[0], F_SETFL, O_NONBLOCK) == 0);
    int efd = eventfd(0, EFD_NONBLOCK);
    CHECK(efd >= 0);
    static int word;
    pthread_t self = pthread_self(), t;
    int before = hits;
    pelting = 1;
    CHECK(pthread_create(&t, 0, pelt, &self) == 0);
    char c;
    uint64_t v = 1;
    for (int i = 0; i < 100000; i++) {
        long r = syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, 0, 0, 0);
        if (r != 0) printf("futex wake: %ld errno %d (round %d)\n", r, errno, i);
        CHECK(r == 0);
        r = read(p[0], &c, 1);
        CHECK(r == -1 && errno == EAGAIN);
        CHECK(write(p[1], "y", 1) == 1);
        CHECK(read(p[0], &c, 1) == 1 && c == 'y');
        CHECK(write(efd, &v, 8) == 8);
        CHECK(read(efd, &v, 8) == 8 && v == 1);
    }
    pelting = 0;
    pthread_join(t, 0);
    CHECK(hits > before);
    close(p[0]), close(p[1]), close(efd);
}

static volatile sig_atomic_t got_chld;
static void on_chld(int sig) { (void)sig; got_chld = 1; }

static void from_another_process(void) {
    // The child signals the parent: a host signal, delivered to the handler.
    install(SIGUSR1, on_signal, 0);
    int before = hits;
    signal(SIGCHLD, on_chld);
    pid_t parent = getpid();
    pid_t child = fork();
    if (child == 0) {
        kill(parent, SIGUSR1);
        _exit(0);
    }
    // How busybox's sh waits for a job: sigsuspend until SIGCHLD's handler
    // has run.
    sigset_t block, old;
    sigemptyset(&block);
    sigaddset(&block, SIGCHLD);
    sigprocmask(SIG_BLOCK, &block, &old);
    while (!got_chld) sigsuspend(&old);
    sigprocmask(SIG_SETMASK, &old, 0);
    CHECK(waitpid(child, 0, 0) == child);
    while (hits == before) usleep(1000);
    CHECK(last_sig == SIGUSR1);
}

static void sigpipe(void) {
    int p[2];
    CHECK(pipe(p) == 0);
    close(p[0]);
    pid_t child = fork();
    if (child == 0) {
        signal(SIGPIPE, SIG_DFL);
        write(p[1], "x", 1);
        _exit(0); // not reached: SIGPIPE kills
    }
    int st;
    waitpid(child, &st, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGPIPE);
    signal(SIGPIPE, SIG_IGN);
    CHECK(write(p[1], "x", 1) == -1 && errno == EPIPE);
    close(p[1]);
}

static volatile sig_atomic_t timer_hits;
static volatile int timer_value, timer_code, timer_id;

static void on_timer(int sig, siginfo_t *info, void *uc) {
    (void)sig, (void)uc;
    timer_value = info->si_value.sival_int;
    timer_code = info->si_code;
    timer_id = info->si_timerid;
    timer_hits++;
}

static void posix_timers(void) {
    int hits;
    // A one-shot timer breaks a blocking read(), with the timer's siginfo.
    install(SIGRTMIN, on_timer, 0);
    struct sigevent ev = {0};
    ev.sigev_notify = SIGEV_SIGNAL;
    ev.sigev_signo = SIGRTMIN;
    ev.sigev_value.sival_int = 42;
    timer_t t;
    CHECK(timer_create(CLOCK_MONOTONIC, &ev, &t) == 0);
    struct itimerspec spec = {.it_value = {0, 20 * 1000000}};
    CHECK(timer_settime(t, 0, &spec, 0) == 0);
    struct itimerspec now;
    CHECK(timer_gettime(t, &now) == 0 && now.it_value.tv_nsec > 0 && now.it_interval.tv_nsec == 0);
    int p[2];
    char c;
    CHECK(pipe(p) == 0);
    CHECK(read(p[0], &c, 1) == -1 && errno == EINTR);
    CHECK(timer_hits == 1 && timer_value == 42 && timer_code == SI_TIMER && timer_id == (int)(intptr_t)t);
    CHECK(timer_gettime(t, &now) == 0 && now.it_value.tv_sec == 0 && now.it_value.tv_nsec == 0);

    // Periodic, absolute start: wakes sigsuspend each period until disarmed.
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    start.tv_nsec += 5 * 1000000;
    if (start.tv_nsec >= 1000000000) start.tv_sec++, start.tv_nsec -= 1000000000;
    spec = (struct itimerspec){.it_interval = {0, 5 * 1000000}, .it_value = start};
    CHECK(timer_settime(t, TIMER_ABSTIME, &spec, 0) == 0);
    sigset_t none;
    sigemptyset(&none);
    while (timer_hits < 4) sigsuspend(&none);
    CHECK(timer_getoverrun(t) >= 0);
    CHECK(timer_delete(t) == 0);

    // sigtimedwait: a blocked timer signal taken without its handler; and a
    // timeout when none comes.
    sigset_t rt, old;
    sigemptyset(&rt);
    sigaddset(&rt, SIGRTMIN);
    sigprocmask(SIG_BLOCK, &rt, &old);
    ev.sigev_value.sival_int = 7;
    CHECK(timer_create(CLOCK_MONOTONIC, &ev, &t) == 0);
    spec = (struct itimerspec){.it_value = {0, 10 * 1000000}};
    CHECK(timer_settime(t, 0, &spec, 0) == 0);
    siginfo_t si;
    hits = timer_hits;
    CHECK(sigwaitinfo(&rt, &si) == SIGRTMIN);
    CHECK(si.si_code == SI_TIMER && si.si_value.sival_int == 7 && timer_hits == hits);
    struct timespec short_wait = {0, 15 * 1000000};
    CHECK(sigtimedwait(&rt, &si, &short_wait) == -1 && errno == EAGAIN);
    CHECK(timer_delete(t) == 0);
    sigprocmask(SIG_SETMASK, &old, 0);
    usleep(20 * 1000);
    hits = timer_hits;
    usleep(20 * 1000);
    CHECK(timer_hits == hits);
    close(p[0]), close(p[1]);
}

int main(void) {
    basics();
    alternate_stack();
    faults();
    timer_interrupts_a_busy_loop();
    restart_and_eintr();
    calls_that_dont_wait();
    from_another_process();
    sigpipe();
    posix_timers();
    puts("signals ok");
    return 0;
}
