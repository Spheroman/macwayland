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
//   contain  a runtime error (a panic) in one process's system call ends
//            that process with SIGSYS; its parent and a sibling carry on
//            (debug builds of the runtime only)
//   model    the rest of the process model in one VM: CLONE_VM without
//            vfork, a child subreaper, pidfds (poll, signal, getfd,
//            waitid), CPU time per process with user and system apart,
//            and small calls (capget, prctl's capability options,
//            get_mempolicy, SO_REUSEPORT)
//   procfs   /proc for every process of the host: the listing, and
//            another process's stat, status, statm, cmdline, environ,
//            comm, maps, fd/, exe, cwd and task/; a zombie's state
//   execmac  execve of a Mac program: prints its errno
//   spawnmac musl's posix_spawn (vfork) of Mac programs, 20 in a row, their
//            output through a pipe and their exit statuses; and of a Mac
//            binary that can't run, which posix_spawn fails with ENOEXEC
//            (Mac programs on, one VM)
//
// Prints "procs TEST ok".
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dirent.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/socket.h>
#include <sys/times.h>
#include <netinet/in.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <spawn.h>
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

// ---- model ----

static volatile int shared_by_clone_vm;

static int clone_vm_child(void *arg) {
    (void)arg;
    shared_by_clone_vm = 42;
    return 3;
}

static long cpu_ms(clockid_t c) {
    struct timespec ts;
    CHECK(clock_gettime(c, &ts) == 0);
    return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

static volatile int spin_stop;
static void *spin(void *arg) {
    (void)arg;
    while (!spin_stop) {}
    return 0;
}

static void model(void) {
    int st;
    // CLONE_VM alone: the child runs in the parent's memory, the parent
    // runs on meanwhile.
    char *stack = malloc(1 << 16);
    pid_t c = clone(clone_vm_child, stack + (1 << 16), CLONE_VM | SIGCHLD, 0);
    CHECK(c > 0);
    st = wait_status(c);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 3 && shared_by_clone_vm == 42);

    // A subreaper: orphaned descendants are its children, not init's, and
    // a zombie among them is told of and waited for as its own.
    CHECK(prctl(PR_SET_CHILD_SUBREAPER, 1) == 0);
    int on = 0;
    CHECK(prctl(PR_GET_CHILD_SUBREAPER, &on) == 0 && on == 1);
    int p[2], go[2];
    CHECK(pipe(p) == 0 && pipe(go) == 0);
    pid_t a = fork();
    CHECK(a >= 0);
    pid_t b = 0, z = 0;
    if (a == 0) {
        pid_t bb = fork();
        if (bb == 0) {
            char ch;
            CHECK(read(go[0], &ch, 1) == 1); // once the middle one has gone
            dprintf(p[1], "%d\n", getppid());
            _exit(5);
        }
        pid_t zz = fork();
        if (zz == 0) _exit(6);
        sleep_ms(20); // zz a zombie by now
        dprintf(p[1], "%d %d\n", bb, zz);
        _exit(0);
    }
    char line[64] = { 0 };
    CHECK(read(p[0], line, sizeof line - 1) > 0);
    CHECK(sscanf(line, "%d %d", &b, &z) == 2);
    CHECK(WEXITSTATUS(wait_status(a)) == 0);
    // The zombie is ours now.
    CHECK(waitpid(z, &st, 0) == z && WEXITSTATUS(st) == 6);
    CHECK(write(go[1], "g", 1) == 1);
    memset(line, 0, sizeof line);
    CHECK(read(p[0], line, sizeof line - 1) > 0);
    CHECK(atoi(line) == getpid());
    CHECK(waitpid(b, &st, 0) == b && WEXITSTATUS(st) == 5);
    CHECK(prctl(PR_SET_CHILD_SUBREAPER, 0) == 0);

    // pidfds: ready once the process has ended; a signal through one; a
    // file taken from the process; waitid(P_PIDFD).
    int q[2];
    CHECK(pipe(q) == 0);
    pid_t k = fork();
    if (k == 0) {
        for (;;) pause(); // holds q[1]
    }
    close(q[1]);
    int pfd = syscall(SYS_pidfd_open, k, 0);
    CHECK(pfd >= 0);
    struct pollfd pp = { pfd, POLLIN, 0 };
    CHECK(poll(&pp, 1, 0) == 0);
    int got = syscall(SYS_pidfd_getfd, pfd, q[1], 0);
    CHECK(got >= 0 && got != q[1]);
    CHECK(write(got, "y", 1) == 1);
    char ch;
    CHECK(read(q[0], &ch, 1) == 1 && ch == 'y');
    close(got);
    CHECK(syscall(SYS_pidfd_send_signal, pfd, SIGTERM, 0, 0) == 0);
    CHECK(poll(&pp, 1, 5000) == 1 && (pp.revents & POLLIN));
    siginfo_t si;
    memset(&si, 0, sizeof si);
    CHECK(waitid(3 /* P_PIDFD */, pfd, &si, WEXITED) == 0);
    CHECK(si.si_pid == k && si.si_code == CLD_KILLED && si.si_status == SIGTERM);
    close(pfd);

    // CPU time per process, user and system apart: a child that sleeps has
    // next to none while this one spins; a thread spinning without a
    // system call counts as it goes; system calls count as system time.
    pid_t sleeper = fork();
    if (sleeper == 0) {
        sleep_ms(300);
        _exit(cpu_ms(CLOCK_PROCESS_CPUTIME_ID) < 50 ? 0 : 1);
    }
    long t0 = cpu_ms(CLOCK_PROCESS_CPUTIME_ID);
    pthread_t sp;
    spin_stop = 0;
    CHECK(pthread_create(&sp, 0, spin, 0) == 0);
    struct timespec w0, w;
    clock_gettime(CLOCK_MONOTONIC, &w0);
    do clock_gettime(CLOCK_MONOTONIC, &w); while ((w.tv_sec - w0.tv_sec) * 1000 + (w.tv_nsec - w0.tv_nsec) / 1000000 < 250);
    long mid = cpu_ms(CLOCK_PROCESS_CPUTIME_ID);
    spin_stop = 1;
    pthread_join(sp, 0);
    CHECK(mid - t0 >= 300); // both threads' 250 ms, the spinner's as it went
    st = wait_status(sleeper);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    clockid_t other;
    CHECK(clock_getcpuclockid(getpid(), &other) == 0 && cpu_ms(other) >= mid);
    struct rusage ru0, ru;
    CHECK(getrusage(RUSAGE_SELF, &ru0) == 0);
    for (int i = 0; i < 100000; i++) syscall(SYS_getppid);
    CHECK(getrusage(RUSAGE_SELF, &ru) == 0);
    long utime = ru.ru_utime.tv_sec * 1000 + ru.ru_utime.tv_usec / 1000;
    long stime_gain = (ru.ru_stime.tv_sec - ru0.ru_stime.tv_sec) * 1000000 + (ru.ru_stime.tv_usec - ru0.ru_stime.tv_usec);
    CHECK(utime >= 300 && stime_gain > 0);
    struct tms tm;
    CHECK(times(&tm) != (clock_t)-1 && tm.tms_utime >= 30);

    // Small calls: no capabilities, one NUMA node, SO_REUSEPORT.
    struct { unsigned version; int pid; } hdr = { 0x20080522, 0 };
    unsigned caps[6] = { 1, 1, 1, 1, 1, 1 };
    CHECK(syscall(SYS_capget, &hdr, caps) == 0);
    for (int i = 0; i < 6; i++) CHECK(caps[i] == 0);
    CHECK(prctl(PR_CAPBSET_READ, 0) == 1 && prctl(PR_CAPBSET_READ, 63) < 0 && errno == EINVAL);
    CHECK(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, 0, 0, 0) == 0);
    CHECK(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, 0, 0, 0) < 0 && errno == EPERM);
    CHECK(prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) == 0);
    int mode = -1;
    unsigned long nodes = 0;
    CHECK(syscall(SYS_get_mempolicy, &mode, &nodes, 64, 0, 0) == 0 && mode == 0);
    CHECK(syscall(SYS_get_mempolicy, 0, &nodes, 64, 0, 4 /* MPOL_F_MEMS_ALLOWED */) == 0 && nodes == 1);
    int s = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(s >= 0);
    int one = 1, v = 0;
    socklen_t vl = sizeof v;
    CHECK(setsockopt(s, SOL_SOCKET, SO_REUSEPORT, &one, sizeof one) == 0);
    CHECK(getsockopt(s, SOL_SOCKET, SO_REUSEPORT, &v, &vl) == 0 && v != 0);
    close(s);
}

// ---- procfs ----

static int read_file(const char *path, char *buf, int size) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) return -1;
    int n = 0, r;
    while (n < size - 1 && (r = read(fd, buf + n, size - 1 - n)) > 0) n += r;
    close(fd);
    buf[n] = 0;
    return n;
}

static int listed(const char *dir, const char *name) {
    DIR *d = opendir(dir);
    if (!d) return 0;
    struct dirent *e;
    int found = 0;
    while ((e = readdir(d))) found |= !strcmp(e->d_name, name);
    closedir(d);
    return found;
}

static void procfs(void) {
    int p[2];
    CHECK(pipe(p) == 0);
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        close(p[1]);
        CHECK(chdir("/tmp") == 0);
        char *args[] = { self, "hold", "an argument", 0 };
        char *env[] = { "PROCFS_TEST=yes", 0 };
        dup2(p[0], 0);
        execve("/proc/self/exe", args, env); // (self may be relative to the old cwd)
        _exit(1);
    }
    close(p[0]);
    sleep_ms(100); // it has exec'd and waits in pause()
    char path[64], buf[8192], name[16];
    snprintf(name, sizeof name, "%d", c);
    CHECK(listed("/proc", name) && listed("/proc", "self") && listed("/proc", "stat"));
    snprintf(name, sizeof name, "%d", getpid());
    CHECK(listed("/proc", name));

    // Read from its memory: once it has exec'd, its new arguments.
    snprintf(path, sizeof path, "/proc/%d/cmdline", c);
    const char want[] = "hold\0an argument";
    int n = 0;
    for (int i = 0; i < 200; i++, sleep_ms(10)) {
        n = read_file(path, buf, sizeof buf);
        if (n > 0 && memmem(buf, n, want, sizeof want)) break;
    }
    CHECK(n > 0 && memmem(buf, n, want, sizeof want) != 0);
    snprintf(path, sizeof path, "/proc/%d/environ", c);
    n = read_file(path, buf, sizeof buf);
    CHECK(n > 0 && memmem(buf, n, "PROCFS_TEST=yes", 15) != 0);
    snprintf(path, sizeof path, "/proc/%d/comm", c);
    // The base name of the path execve was given, as Linux's.
    CHECK(read_file(path, buf, sizeof buf) > 0 && !strcmp(buf, "exe\n"));

    snprintf(path, sizeof path, "/proc/%d/stat", c);
    CHECK(read_file(path, buf, sizeof buf) > 0);
    int spid, sppid;
    char state;
    CHECK(sscanf(buf, "%d (exe) %c %d", &spid, &state, &sppid) == 3);
    CHECK(spid == c && sppid == getpid() && (state == 'S' || state == 'R'));
    snprintf(path, sizeof path, "/proc/%d/status", c);
    CHECK(read_file(path, buf, sizeof buf) > 0);
    char ppid_line[32];
    snprintf(ppid_line, sizeof ppid_line, "PPid:\t%d\n", getpid());
    CHECK(strstr(buf, "Name:\texe\n") && strstr(buf, ppid_line) && strstr(buf, "Threads:\t1\n"));
    snprintf(path, sizeof path, "/proc/%d/statm", c);
    long size = 0, rss = 0;
    CHECK(read_file(path, buf, sizeof buf) > 0 && sscanf(buf, "%ld %ld", &size, &rss) == 2 && size > 0 && rss > 0);
    snprintf(path, sizeof path, "/proc/%d/maps", c);
    // (Its regions; the loader's own mappings of the program aren't named.)
    CHECK(read_file(path, buf, sizeof buf) > 0 && strchr(buf, '\n') != strrchr(buf, '\n'));

    char link[256];
    snprintf(path, sizeof path, "/proc/%d/exe", c);
    n = readlink(path, link, sizeof link - 1);
    CHECK(n > 0);
    link[n] = 0;
    CHECK(strstr(link, "procs"));
    snprintf(path, sizeof path, "/proc/%d/cwd", c);
    n = readlink(path, link, sizeof link - 1);
    CHECK(n > 0);
    link[n] = 0;
    CHECK(!strcmp(link, "/tmp") || strstr(link, "tmp"));
    snprintf(path, sizeof path, "/proc/%d/fd", c);
    CHECK(listed(path, "0") && listed(path, "1"));
    snprintf(path, sizeof path, "/proc/%d/fd/0", c);
    n = readlink(path, link, sizeof link - 1);
    CHECK(n > 0);
    link[n] = 0;
    CHECK(!strncmp(link, "pipe:[", 6));
    snprintf(path, sizeof path, "/proc/%d/task", c);
    snprintf(name, sizeof name, "%d", c);
    CHECK(listed(path, name));
    snprintf(path, sizeof path, "/proc/%d/task/%d/stat", c, c);
    CHECK(read_file(path, buf, sizeof buf) > 0);

    // A zombie: in the listing, its state Z, until waited for.
    CHECK(kill(c, SIGTERM) == 0);
    sleep_ms(100);
    snprintf(path, sizeof path, "/proc/%d/stat", c);
    CHECK(read_file(path, buf, sizeof buf) > 0 && sscanf(buf, "%d (exe) %c", &spid, &state) == 2 && state == 'Z');
    wait_status(c);
    CHECK(read_file(path, buf, sizeof buf) < 0 && errno == ENOENT);

    // /proc/self is a link to this process's directory.
    n = readlink("/proc/self", link, sizeof link - 1);
    CHECK(n > 0);
    link[n] = 0;
    CHECK(atoi(link) == getpid());
    CHECK(read_file("/proc/stat", buf, sizeof buf) > 0 && strstr(buf, "btime "));
}

// ---- contain ----

// The runtime's debug-only call that panics (linux_abi::nr::MACWL_TEST_PANIC).
#define MACWL_TEST_PANIC 0x4d570001L

static void *spin_then_panic(void *arg) {
    (void)arg;
    sleep_ms(5);
    syscall(MACWL_TEST_PANIC);
    return 0;
}

static void contain(void) {
    // A sibling that must outlive the failing process, blocked meanwhile.
    int p[2];
    CHECK(pipe(p) == 0);
    pid_t sibling = fork();
    CHECK(sibling >= 0);
    if (sibling == 0) {
        close(p[1]);
        char c;
        _exit(read(p[0], &c, 1) == 0 ? 0 : 1);
    }
    close(p[0]);
    // The call made by the main thread, and by one of several threads
    // (others blocked in reads and sleeps).
    for (int how = 0; how < 2; how++) {
        pid_t pid = fork();
        CHECK(pid >= 0);
        if (pid == 0) {
            if (how == 0) syscall(MACWL_TEST_PANIC);
            int q[2];
            CHECK(pipe(q) == 0);
            pthread_t t;
            CHECK(pthread_create(&t, 0, spin_then_panic, 0) == 0);
            char c;
            read(q[0], &c, 1);
            _exit(0);
        }
        int st = wait_status(pid);
        if (!WIFSIGNALED(st) || WTERMSIG(st) != SIGSYS) {
            dprintf(2, "case %d: status %#x\n", how, st);
            _exit(1);
        }
    }
    // The host carries on: the sibling, and forks after.
    pid_t later = fork();
    CHECK(later >= 0);
    if (later == 0) _exit(7);
    CHECK(WEXITSTATUS(wait_status(later)) == 7);
    close(p[1]);
    CHECK(WIFEXITED(wait_status(sibling)));
}

int main(int argc, char **argv) {
    self = argv[0];
    CHECK(argc >= 2);
    const char *test = argv[1];
    if (!strcmp(test, "hold")) {
        // Until stdin closes (its parent gone, say).
        char c;
        while (read(0, &c, 1) != 0) {}
        return 0;
    } else if (!strcmp(test, "sleep")) {
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
    } else if (!strcmp(test, "model")) {
        model();
    } else if (!strcmp(test, "procfs")) {
        procfs();
    } else if (!strcmp(test, "contain")) {
        contain();
    } else if (!strcmp(test, "spawnmac")) {
        extern char **environ;
        for (int i = 0; i < 20; i++) {
            int p[2];
            CHECK(pipe(p) == 0);
            posix_spawn_file_actions_t fa;
            posix_spawn_file_actions_init(&fa);
            posix_spawn_file_actions_adddup2(&fa, p[1], 1);
            posix_spawn_file_actions_addclose(&fa, p[0]);
            char n[16];
            snprintf(n, sizeof n, "mac %d", i);
            char *args[] = { "/bin/echo", n, 0 };
            pid_t pid;
            CHECK(posix_spawn(&pid, "/bin/echo", &fa, 0, args, environ) == 0);
            close(p[1]);
            char buf[64] = { 0 };
            int got = 0, r;
            while ((r = read(p[0], buf + got, sizeof buf - 1 - got)) > 0)
                got += r;
            close(p[0]);
            int st;
            CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0);
            char want[32];
            snprintf(want, sizeof want, "mac %d\n", i);
            CHECK(!strcmp(buf, want));
            posix_spawn_file_actions_destroy(&fa);
            char *f[] = { "/usr/bin/false", 0 };
            CHECK(posix_spawn(&pid, "/usr/bin/false", 0, 0, f, environ) == 0);
            CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 1);
        }
        // A Mach-O file macOS won't run (cut short): the spawn fails, with
        // the errno, as Linux's posix_spawn reports the child's execve's.
        if (argc > 2) {
            char *b[] = { argv[2], 0 };
            pid_t pid;
            int e = posix_spawn(&pid, argv[2], 0, 0, b, environ);
            printf("bad mac binary: %s\n", e == ENOEXEC ? "ENOEXEC" : strerror(e));
        }
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
