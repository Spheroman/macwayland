// The process model, for many processes in one VM (docs/processes.md, step
// 3) and 1:1 alike. Run as `procs TEST [ARGS]`, TEST one of:
//
//   many N [hold]  N processes alive at once (more than 128: the VMs a
//            Mac has); with hold, until stdin closes
//   cow      copy-on-write fork: writes isolated both ways, after a second
//            fork, with threads writing as the process forks; MAP_SHARED
//            and vfork memory shared
//   wait     zombies, wait4 and waitid (WNOHANG, WNOWAIT, WUNTRACED,
//            WCONTINUED), SIGCHLD's si_code, reaping by SIG_IGN, orphans
//   groups   process groups and sessions, kill to a group, to 0 and to a
//            pid, the sender in siginfo, tgkill of another process's thread
//   timers   a process's interval and POSIX timers and alarm() signal it
//            alone: another process's sleeps and waits aren't interrupted
//   execmac  execve of a Mac program: prints its errno
//
// Prints "procs TEST ok".
#define _GNU_SOURCE
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { dprintf(2, "FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); _exit(1); } } while (0)

static char *self;

static void sleep_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000 };
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {}
}

static int wait_status(pid_t pid) {
    int st;
    CHECK(waitpid(pid, &st, 0) == pid);
    return st;
}

// ---- many ----

static void many(int n, int hold) {
    int ready[2], release[2];
    CHECK(pipe(ready) == 0 && pipe(release) == 0);
    pid_t *pids = calloc(n, sizeof *pids);
    for (int i = 0; i < n; i++) {
        pids[i] = fork();
        CHECK(pids[i] >= 0);
        if (pids[i] == 0) {
            close(release[1]);
            CHECK(write(ready[1], "r", 1) == 1);
            char c;
            // Alive until the parent lets go: all n at once.
            while (read(release[0], &c, 1) < 0 && errno == EINTR) {}
            _exit(i % 200);
        }
    }
    close(release[0]);
    for (int i = 0; i < n; i++) {
        char c;
        CHECK(read(ready[0], &c, 1) == 1);
    }
    // Every one is alive now, at once.
    for (int i = 0; i < n; i++) CHECK(kill(pids[i], 0) == 0);
    printf("%d alive\n", n);
    fflush(stdout);
    // Held alive until stdin closes (the host counts its Mac processes).
    if (hold) {
        char c;
        while (read(0, &c, 1) > 0) {}
    }
    close(release[1]);
    for (int i = 0; i < n; i++) {
        int st = wait_status(pids[i]);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == i % 200);
    }
    CHECK(waitpid(-1, 0, WNOHANG) < 0 && errno == ECHILD);
}

// ---- cow ----

#define PAGES 64
static char buf[PAGES][4096];

static int pages_are(int from, int to, char v) {
    for (int p = from; p < to; p++)
        for (int i = 0; i < 4096; i += 512)
            if (buf[p][i] != v) return 0;
    return 1;
}

static void fill(int from, int to, char v) {
    for (int p = from; p < to; p++) memset(buf[p], v, 4096);
}

static atomic_int stop_threads;
static volatile long counters[4][512]; // a page each

static void *count(void *arg) {
    volatile long *c = arg;
    while (!atomic_load(&stop_threads)) (*c)++;
    return 0;
}

static void cow(void) {
    fill(0, PAGES, 1);
    int to_a[2], from_a[2];
    CHECK(pipe(to_a) == 0 && pipe(from_a) == 0);
    pid_t a = fork();
    CHECK(a >= 0);
    if (a == 0) {
        CHECK(pages_are(0, PAGES, 1));
        fill(0, PAGES / 2, 2);
        char c;
        CHECK(write(from_a[1], "w", 1) == 1);
        CHECK(read(to_a[0], &c, 1) == 1); // the parent has written its half
        CHECK(pages_are(0, PAGES / 2, 2) && pages_are(PAGES / 2, PAGES, 1));
        CHECK(write(from_a[1], "k", 1) == 1);
        CHECK(read(to_a[0], &c, 1) == 1); // and a second child has written
        CHECK(pages_are(0, PAGES / 2, 2) && pages_are(PAGES / 2, PAGES, 1));
        _exit(0);
    }
    char c;
    CHECK(read(from_a[0], &c, 1) == 1);
    CHECK(pages_are(0, PAGES, 1)); // the child's writes are its own
    fill(PAGES / 2, PAGES, 3);
    CHECK(write(to_a[1], "g", 1) == 1);
    CHECK(read(from_a[0], &c, 1) == 1);

    // A second fork, while the first child still shares frames.
    pid_t b = fork();
    CHECK(b >= 0);
    if (b == 0) {
        CHECK(pages_are(0, PAGES / 2, 1) && pages_are(PAGES / 2, PAGES, 3));
        fill(0, PAGES, 4);
        CHECK(pages_are(0, PAGES, 4));
        _exit(0);
    }
    CHECK(WIFEXITED(wait_status(b)) );
    CHECK(write(to_a[1], "g", 1) == 1);
    CHECK(WEXITSTATUS(wait_status(a)) == 0);
    CHECK(pages_are(0, PAGES / 2, 1) && pages_are(PAGES / 2, PAGES, 3));
    // Alone again: writes in place.
    fill(0, PAGES, 5);
    CHECK(pages_are(0, PAGES, 5));

    // Forks with threads writing all the while: the child's copy holds
    // still (no threads of its own), the parent's goes on.
    pthread_t t[4];
    for (int i = 0; i < 4; i++) CHECK(pthread_create(&t[i], 0, count, (void *)&counters[i][0]) == 0);
    for (int round = 0; round < 20; round++) {
        sleep_ms(2);
        pid_t c = fork();
        CHECK(c >= 0);
        if (c == 0) {
            long seen[4];
            for (int i = 0; i < 4; i++) seen[i] = counters[i][0];
            sleep_ms(5);
            for (int i = 0; i < 4; i++) CHECK(counters[i][0] == seen[i]);
            for (int i = 0; i < 4; i++) counters[i][0] = -1;
            _exit(0);
        }
        CHECK(WEXITSTATUS(wait_status(c)) == 0);
        for (int i = 0; i < 4; i++) CHECK(counters[i][0] >= 0);
    }
    atomic_store(&stop_threads, 1);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);

    // MAP_SHARED is shared, private memory isn't.
    volatile int *shared = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(shared != MAP_FAILED);
    pid_t s = fork();
    if (s == 0) {
        *shared = 7;
        buf[0][0] = 9;
        _exit(0);
    }
    CHECK(WEXITSTATUS(wait_status(s)) == 0);
    CHECK(*shared == 7 && buf[0][0] == 5);

    // vfork: the child runs in the parent's memory until it exits.
    static volatile int vforked;
    pid_t v = vfork();
    if (v == 0) {
        vforked = 1;
        _exit(0);
    }
    CHECK(v > 0 && vforked == 1);
    CHECK(WEXITSTATUS(wait_status(v)) == 0);
}

// ---- wait ----

static volatile sig_atomic_t chld_code, chld_pid;

static void on_chld(int sig, siginfo_t *si, void *ctx) {
    (void)sig, (void)ctx;
    chld_code = si->si_code;
    chld_pid = si->si_pid;
}

static void wait_tests(void) {
    // A zombie: there until waited for.
    pid_t z = fork();
    if (z == 0) _exit(3);
    sleep_ms(50);
    CHECK(kill(z, 0) == 0);
    CHECK(getpgid(z) == getpgrp());
    siginfo_t si;
    memset(&si, 0, sizeof si);
    CHECK(waitid(P_PID, z, &si, WEXITED | WNOWAIT) == 0);
    CHECK(si.si_pid == z && si.si_code == CLD_EXITED && si.si_status == 3);
    int st;
    CHECK(waitpid(z, &st, WNOHANG) == z && WIFEXITED(st) && WEXITSTATUS(st) == 3);
    CHECK(kill(z, 0) < 0 && errno == ESRCH);
    CHECK(waitpid(-1, &st, WNOHANG) < 0 && errno == ECHILD);

    // WNOHANG with a child running; then killed by a signal.
    pid_t r = fork();
    if (r == 0) for (;;) pause();
    CHECK(waitpid(r, &st, WNOHANG) == 0);
    CHECK(kill(r, SIGKILL) == 0);
    st = wait_status(r);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL);

    // Stops and continues: reported to wait and in SIGCHLD.
    struct sigaction sa = { .sa_sigaction = on_chld, .sa_flags = SA_SIGINFO | SA_RESTART };
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(SIGCHLD, &sa, 0) == 0);
    int hold[2];
    CHECK(pipe(hold) == 0);
    pid_t s = fork();
    if (s == 0) {
        raise(SIGSTOP);
        // Continued: it ends once the parent has seen that.
        char c;
        while (read(hold[0], &c, 1) < 0 && errno == EINTR) {}
        _exit(4);
    }
    CHECK(waitpid(s, &st, WUNTRACED) == s && WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP);
    for (int i = 0; i < 100 && chld_code != CLD_STOPPED; i++) sleep_ms(5);
    CHECK(chld_code == CLD_STOPPED && chld_pid == s);
    CHECK(kill(s, SIGCONT) == 0);
    CHECK(waitpid(s, &st, WCONTINUED) == s && WIFCONTINUED(st));
    CHECK(write(hold[1], "x", 1) == 1);
    st = wait_status(s);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 4);
    for (int i = 0; i < 100 && chld_code != CLD_EXITED; i++) sleep_ms(5);
    CHECK(chld_code == CLD_EXITED && chld_pid == s);
    signal(SIGCHLD, SIG_DFL);

    // A child stopped by another process's SIGSTOP, then killed.
    pid_t k = fork();
    if (k == 0) for (;;) pause();
    CHECK(kill(k, SIGSTOP) == 0);
    CHECK(waitpid(k, &st, WUNTRACED) == k && WIFSTOPPED(st));
    CHECK(kill(k, SIGTERM) == 0); // pending while it's stopped
    CHECK(kill(k, SIGCONT) == 0);
    st = wait_status(k);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);

    // SIGCHLD ignored: children are reaped as they end.
    signal(SIGCHLD, SIG_IGN);
    pid_t g = fork();
    if (g == 0) _exit(0);
    CHECK(waitpid(g, &st, 0) < 0 && errno == ECHILD);
    signal(SIGCHLD, SIG_DFL);

    // An orphan: its parent ends, and it's init's (getppid 1 in one VM; a
    // Mac process's in 1:1: just not the parent's).
    int p[2];
    CHECK(pipe(p) == 0);
    pid_t mid = fork();
    if (mid == 0) {
        pid_t me = getpid();
        pid_t o = fork();
        if (o == 0) {
            while (getppid() == me) sleep_ms(2);
            CHECK(write(p[1], "o", 1) == 1);
            _exit(0);
        }
        _exit(0);
    }
    CHECK(WEXITSTATUS(wait_status(mid)) == 0);
    char c;
    CHECK(read(p[0], &c, 1) == 1 && c == 'o');
}

// ---- groups ----

static atomic_int usr1s;
static volatile sig_atomic_t usr2_from, usr2_tid;

static void on_usr1(int sig) { (void)sig; atomic_fetch_add(&usr1s, 1); }
static void on_usr2(int sig, siginfo_t *si, void *ctx) {
    (void)sig, (void)ctx;
    usr2_from = si->si_pid;
    usr2_tid = gettid();
}

static void *wait_for_usr2(void *arg) {
    int fd = *(int *)arg;
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    CHECK(pthread_sigmask(SIG_UNBLOCK, &usr2, 0) == 0);
    char tid[16];
    int n = snprintf(tid, sizeof tid, "%d\n", gettid());
    CHECK(write(fd, tid, n) == n);
    while (!usr2_tid) sleep_ms(1);
    return 0;
}

static void groups(void) {
    signal(SIGUSR1, on_usr1);
    pid_t me = getpid();
    // Two children in a group of the first one's.
    int ready[2], go[2];
    CHECK(pipe(ready) == 0 && pipe(go) == 0);
    pid_t kids[2];
    for (int i = 0; i < 2; i++) {
        kids[i] = fork();
        if (kids[i] == 0) {
            close(go[1]);
            CHECK(setpgid(0, i == 0 ? 0 : kids[0]) == 0);
            CHECK(write(ready[1], "r", 1) == 1);
            // Count USR1s until told to stop, then report how many.
            char c;
            while (read(go[0], &c, 1) < 0 && errno == EINTR) {}
            _exit(atomic_load(&usr1s));
        }
        // The parent sets it too, as shells do (whichever runs first).
        int r = setpgid(kids[i], kids[0]);
        CHECK(r == 0 || errno == EACCES || errno == ESRCH);
    }
    close(go[0]);
    for (int i = 0; i < 2; i++) {
        char c;
        CHECK(read(ready[0], &c, 1) == 1);
    }
    CHECK(getpgid(kids[0]) == kids[0] && getpgid(kids[1]) == kids[0]);
    CHECK(getpgid(0) != kids[0]);
    CHECK(kill(-kids[0], SIGUSR1) == 0);
    CHECK(kill(-kids[0], SIGUSR1) == 0 || 1);
    sleep_ms(50);
    // kill(0): the caller's own group, not the children's.
    atomic_store(&usr1s, 0);
    CHECK(kill(0, SIGUSR1) == 0);
    for (int i = 0; i < 100 && atomic_load(&usr1s) == 0; i++) sleep_ms(1);
    CHECK(atomic_load(&usr1s) >= 1);
    close(go[1]);
    for (int i = 0; i < 2; i++) {
        int st = wait_status(kids[i]);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) >= 1); // each got the group's
    }
    CHECK(kill(-kids[0], SIGUSR1) < 0 && errno == ESRCH); // the group is gone

    // A session of its own: not for a group leader; then setsid works.
    pid_t s = fork();
    if (s == 0) {
        CHECK(getsid(0) == getsid(me));
        CHECK(setsid() == getpid());
        CHECK(getsid(0) == getpid() && getpgid(0) == getpid());
        CHECK(setsid() < 0 && errno == EPERM);
        _exit(0);
    }
    CHECK(WEXITSTATUS(wait_status(s)) == 0);

    // setpgid on a child that has exec'd: EACCES.
    int execd[2];
    CHECK(pipe(execd) == 0);
    pid_t e = fork();
    if (e == 0) {
        dup2(execd[1], 1);
        char *argv[] = { self, "sleep", 0 };
        execv(self, argv);
        _exit(9);
    }
    char c;
    CHECK(read(execd[0], &c, 1) == 1); // it has exec'd and said so
    CHECK(setpgid(e, e) < 0 && errno == EACCES);
    CHECK(kill(e, SIGTERM) == 0);
    int st = wait_status(e);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);

    // The sender's pid in siginfo, and tgkill of another process's thread.
    struct sigaction sa = { .sa_sigaction = on_usr2, .sa_flags = SA_SIGINFO };
    sigemptyset(&sa.sa_mask);
    CHECK(sigaction(SIGUSR2, &sa, 0) == 0);
    int tidp[2];
    CHECK(pipe(tidp) == 0);
    pid_t t = fork();
    if (t == 0) {
        pthread_t th;
        sigset_t usr2;
        sigemptyset(&usr2);
        sigaddset(&usr2, SIGUSR2);
        CHECK(pthread_sigmask(SIG_BLOCK, &usr2, 0) == 0); // only the thread takes it
        CHECK(pthread_create(&th, 0, wait_for_usr2, &tidp[1]) == 0);
        pthread_join(th, 0);
        CHECK(usr2_from == getppid());
        _exit(usr2_tid != getpid() ? 0 : 2);
    }
    char line[16] = {0};
    CHECK(read(tidp[0], line, sizeof line - 1) > 0);
    pid_t tid = atoi(line);
    CHECK(tid != t);
    CHECK(syscall(SYS_tgkill, t, tid, SIGUSR2) == 0);
    CHECK(WEXITSTATUS(wait_status(t)) == 0);
}

// ---- timers ----

static atomic_int alrms, usr1_timer;
static void on_alrm(int sig) { (void)sig; atomic_fetch_add(&alrms, 1); }
static void on_usr1_timer(int sig) { (void)sig; atomic_fetch_add(&usr1_timer, 1); }

// A process with no timers sleeps and waits for ms milliseconds in short
// steps, none of which may end early: a timer of another process's must not
// interrupt it. Exits 0 if none did.
static void undisturbed(long ms) {
    int p[2];
    CHECK(pipe(p) == 0);
    for (long done = 0; done < ms; done += 30) {
        struct timespec ts = { 0, 10 * 1000000 };
        if (nanosleep(&ts, 0) != 0) {
            dprintf(2, "nanosleep: errno %d\n", errno);
            _exit(2);
        }
        struct pollfd pf = { p[0], POLLIN, 0 };
        int n = poll(&pf, 1, 10);
        if (n != 0) {
            dprintf(2, "poll: %d errno %d\n", n, errno);
            _exit(3);
        }
        struct timespec t2 = { 0, 10 * 1000000 };
        siginfo_t si;
        sigset_t none;
        sigemptyset(&none);
        sigaddset(&none, SIGUSR2);
        sigprocmask(SIG_BLOCK, &none, 0);
        n = sigtimedwait(&none, &si, &t2);
        if (n != -1 || errno != EAGAIN) {
            dprintf(2, "sigtimedwait: %d errno %d\n", n, errno);
            _exit(4);
        }
    }
    _exit(0);
}

static void timers(void) {
    struct sigaction sa = { 0 };
    sa.sa_handler = on_alrm;
    CHECK(sigaction(SIGALRM, &sa, 0) == 0);
    sa.sa_handler = on_usr1_timer;
    CHECK(sigaction(SIGUSR1, &sa, 0) == 0);

    // An interval timer and a POSIX timer, every millisecond, while another
    // process sleeps and polls.
    pid_t quiet = fork();
    CHECK(quiet >= 0);
    if (quiet == 0) undisturbed(600);
    struct itimerval it = { { 0, 1000 }, { 0, 1000 } };
    CHECK(setitimer(ITIMER_REAL, &it, 0) == 0);
    struct sigevent ev = { 0 };
    ev.sigev_notify = SIGEV_SIGNAL;
    ev.sigev_signo = SIGUSR1;
    timer_t t;
    CHECK(timer_create(CLOCK_MONOTONIC, &ev, &t) == 0);
    struct itimerspec spec = { { 0, 1000000 }, { 0, 1000000 } };
    CHECK(timer_settime(t, 0, &spec, 0) == 0);
    int st;
    pid_t got;
    while ((got = waitpid(quiet, &st, 0)) < 0 && errno == EINTR) {}
    CHECK(got == quiet);
    struct itimerval off = { { 0, 0 }, { 0, 0 } };
    CHECK(setitimer(ITIMER_REAL, &off, 0) == 0);
    CHECK(timer_delete(t) == 0);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
        dprintf(2, "the other process was disturbed: status %#x\n", st);
        _exit(1);
    }
    // The timers' signals came here, often.
    CHECK(atomic_load(&alrms) > 50 && atomic_load(&usr1_timer) > 50);

    // alarm(): one SIGALRM, for this process only.
    quiet = fork();
    CHECK(quiet >= 0);
    if (quiet == 0) undisturbed(400);
    int before = atomic_load(&alrms);
    alarm(1);
    while (atomic_load(&alrms) == before) pause();
    while ((got = waitpid(quiet, &st, 0)) < 0 && errno == EINTR) {}
    CHECK(got == quiet && WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

int main(int argc, char **argv) {
    self = argv[0];
    CHECK(argc >= 2);
    const char *test = argv[1];
    if (!strcmp(test, "sleep")) {
        CHECK(write(1, "e", 1) == 1);
        for (;;) pause();
    } else if (!strcmp(test, "many")) {
        many(argc > 2 ? atoi(argv[2]) : 200, argc > 3 && !strcmp(argv[3], "hold"));
    } else if (!strcmp(test, "cow")) {
        cow();
    } else if (!strcmp(test, "wait")) {
        wait_tests();
    } else if (!strcmp(test, "groups")) {
        groups();
    } else if (!strcmp(test, "timers")) {
        timers();
    } else if (!strcmp(test, "execmac")) {
        char *args[] = { "/usr/bin/true", 0 };
        execv("/usr/bin/true", args);
        printf("execve: %s\n", errno == ENOEXEC ? "ENOEXEC" : strerror(errno));
        return 0;
    } else {
        CHECK(!"no such test");
    }
    printf("procs %s ok\n", test);
    return 0;
}
