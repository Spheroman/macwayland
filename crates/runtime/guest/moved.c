// What a process takes along when it moves to a host of its own at its
// execve (docs/processes.md, step 9): a forked child execs this program
// again with MACWL_NEW_HOST=1, and the exec'd program checks what it got.
//
//   pending   a signal blocked and pending across the exec is still
//             pending, and handled once unblocked
//   fds       an eventfd (its count), a timerfd (armed), an epoll set
//             (watching the eventfd, with its data), a signalfd (its
//             signals) and an inotify instance (its watch) keep working
//   ids       getpgid and getsid of its parent, in the host it came from
//   pidfd     a pidfd for a sibling left in the old host still names it
//             (a signal through it, ready once it ends), and pidfd_open
//             of its parent there works
//
// Prints "moved TEST ok".
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/inotify.h>
#include <poll.h>
#include <sys/syscall.h>
#include <sys/signalfd.h>
#include <sys/timerfd.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { printf("FAIL line %d: %s (errno %d)\n", __LINE__, #c, errno); fflush(stdout); _exit(1); } } while (0)

static char *self;

// Fork, and exec this program as `child ARGS` in a host of its own (the
// child calling `before` first); its exit status.
static int run_moved(char *const args[], void (*before)(void)) {
    pid_t c = fork();
    CHECK(c >= 0);
    if (c == 0) {
        if (before) before();
        // (MOVED_STAY=1: the same, without moving, to compare.)
        char *env[] = {getenv("MOVED_STAY") ? "MOVED_STAY=1" : "MACWL_NEW_HOST=1", NULL};
        execve(self, args, env);
        _exit(127);
    }
    int st;
    CHECK(waitpid(c, &st, 0) == c);
    return WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st);
}

static volatile sig_atomic_t handled;
static void on_usr1(int sig) { (void)sig; handled++; }

// In the forked child (a fork's child has nothing pending): SIGUSR1
// blocked and pending as it execs, once for the thread (raise) and once
// for the process (kill).
static void block_and_raise(void) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigaddset(&s, SIGUSR2);
    CHECK(sigprocmask(SIG_BLOCK, &s, NULL) == 0);
    CHECK(raise(SIGUSR1) == 0);
    CHECK(kill(getpid(), SIGUSR2) == 0);
}

static void pending(void) {
    char *args[] = {self, "pending-child", NULL};
    CHECK(run_moved(args, block_and_raise) == 0);
}

static void pending_child(void) {
    sigset_t s;
    CHECK(sigpending(&s) == 0);
    CHECK(sigismember(&s, SIGUSR1) && sigismember(&s, SIGUSR2));
    signal(SIGUSR1, on_usr1);
    signal(SIGUSR2, on_usr1);
    sigemptyset(&s);
    sigaddset(&s, SIGUSR1);
    sigaddset(&s, SIGUSR2);
    CHECK(sigprocmask(SIG_UNBLOCK, &s, NULL) == 0);
    CHECK(handled == 2);
}

static void fds(void) {
    int ev = eventfd(3, 0);
    CHECK(ev >= 0);
    int tm = timerfd_create(CLOCK_MONOTONIC, 0);
    CHECK(tm >= 0);
    struct itimerspec its = {{0, 0}, {0, 50 * 1000 * 1000}};
    CHECK(timerfd_settime(tm, 0, &its, NULL) == 0);
    int ep = epoll_create1(0);
    CHECK(ep >= 0);
    struct epoll_event e = {.events = EPOLLIN, .data.u64 = 0x1234};
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, ev, &e) == 0);
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGUSR2);
    CHECK(sigprocmask(SIG_BLOCK, &s, NULL) == 0);
    int sf = signalfd(-1, &s, 0);
    CHECK(sf >= 0);
    char dir[] = "/tmp/moved.XXXXXX";
    CHECK(mkdtemp(dir) != NULL);
    int in = inotify_init1(0);
    CHECK(in >= 0);
    int wd = inotify_add_watch(in, dir, IN_CREATE);
    CHECK(wd >= 0);
    char a[6][32];
    snprintf(a[0], 32, "%d", ev);
    snprintf(a[1], 32, "%d", tm);
    snprintf(a[2], 32, "%d", ep);
    snprintf(a[3], 32, "%d", sf);
    snprintf(a[4], 32, "%d", in);
    snprintf(a[5], 32, "%d", wd);
    char *args[] = {self, "fds-child", a[0], a[1], a[2], a[3], a[4], a[5], dir, NULL};
    CHECK(run_moved(args, NULL) == 0);
    rmdir(dir);
}

static void fds_child(char **argv) {
    int ev = atoi(argv[2]), tm = atoi(argv[3]), ep = atoi(argv[4]), sf = atoi(argv[5]), in = atoi(argv[6]), wd = atoi(argv[7]);
    const char *dir = argv[8];
    // The epoll set sees the eventfd ready, with its data.
    struct epoll_event e;
    CHECK(epoll_wait(ep, &e, 1, 1000) == 1);
    CHECK(e.data.u64 == 0x1234 && (e.events & EPOLLIN));
    uint64_t n;
    CHECK(read(ev, &n, 8) == 8 && n == 3);
    // The timer fires (it was armed for 50 ms from before the exec).
    CHECK(read(tm, &n, 8) == 8 && n >= 1);
    // The signalfd reads the signals it watches.
    CHECK(raise(SIGUSR2) == 0);
    struct signalfd_siginfo si;
    CHECK(read(sf, &si, sizeof si) == sizeof si && si.ssi_signo == SIGUSR2);
    // The inotify watch reports a file made in its directory.
    char path[256];
    snprintf(path, sizeof path, "%s/new", dir);
    int f = open(path, O_CREAT | O_WRONLY, 0600);
    CHECK(f >= 0);
    close(f);
    char buf[256];
    ssize_t got = read(in, buf, sizeof buf);
    struct inotify_event *ie = (struct inotify_event *)buf;
    CHECK(got >= (ssize_t)sizeof *ie && ie->wd == wd && (ie->mask & IN_CREATE) && !strcmp(ie->name, "new"));
    unlink(path);
}

static void ids(void) {
    // A group and session of its own, so they're told from the defaults.
    setsid();
    char g[32], s[32];
    snprintf(g, 32, "%d", getpgid(0));
    snprintf(s, 32, "%d", getsid(0));
    char *args[] = {self, "ids-child", g, s, NULL};
    CHECK(run_moved(args, NULL) == 0);
}

static void ids_child(char **argv) {
    pid_t parent = getppid();
    CHECK(getpgid(parent) == atoi(argv[2]));
    CHECK(getsid(parent) == atoi(argv[3]));
    // Its own: what it had, its parent's.
    CHECK(getpgid(0) == atoi(argv[2]) && getsid(0) == atoi(argv[3]));
    CHECK(getpgid(4000000) < 0 && errno == ESRCH);
}

static int pidfd_open(pid_t pid) { return syscall(434, pid, 0); }
static int pidfd_send_signal(int fd, int sig) { return syscall(424, fd, sig, NULL, 0); }

// Whether pidfd `fd` reads as ready (its process ended) within `ms`.
static int ended(int fd, int ms) {
    struct pollfd p = {.fd = fd, .events = POLLIN};
    return poll(&p, 1, ms) == 1 && (p.revents & POLLIN);
}

static void pidfd(void) {
    pid_t sib = fork();
    CHECK(sib >= 0);
    if (sib == 0) {
        // (Not for ever, should the child fail before it signals.)
        alarm(20);
        pause();
        _exit(0);
    }
    int fd = pidfd_open(sib);
    CHECK(fd >= 0);
    CHECK(fcntl(fd, F_SETFD, 0) == 0);
    char a[2][32];
    snprintf(a[0], 32, "%d", fd);
    snprintf(a[1], 32, "%d", sib);
    char *args[] = {self, "pidfd-child", a[0], a[1], NULL};
    int r = run_moved(args, NULL);
    if (r != 0) kill(sib, SIGKILL);
    CHECK(r == 0);
    // The child's SIGTERM through the pidfd it took along ended it.
    int st;
    CHECK(waitpid(sib, &st, 0) == sib && WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);
    CHECK(ended(fd, 0));
}

static void pidfd_child(char **argv) {
    int fd = atoi(argv[2]);
    (void)argv[3];
    // The sibling's pidfd came along, not ready: the sibling is alive.
    CHECK(fcntl(fd, F_GETFD) >= 0);
    CHECK(!ended(fd, 0));
    // One for its parent, in the host it came from; none for a pid no
    // host has.
    int parent = pidfd_open(getppid());
    CHECK(parent >= 0);
    CHECK(!ended(parent, 0));
    CHECK(pidfd_open(4000000) < 0 && errno == ESRCH);
    // A signal through it ends the sibling, and it reads as ready.
    CHECK(pidfd_send_signal(fd, SIGTERM) == 0);
    CHECK(ended(fd, 5000));
    CHECK(!ended(parent, 0));
}

int main(int argc, char **argv) {
    self = argv[0];
    setvbuf(stdout, NULL, _IONBF, 0);
    CHECK(argc >= 2);
    if (!strcmp(argv[1], "pending")) {
        pending();
    } else if (!strcmp(argv[1], "pending-child")) {
        pending_child();
        return 0;
    } else if (!strcmp(argv[1], "fds")) {
        fds();
    } else if (!strcmp(argv[1], "ids")) {
        ids();
    } else if (!strcmp(argv[1], "ids-child")) {
        ids_child(argv);
        return 0;
    } else if (!strcmp(argv[1], "pidfd")) {
        pidfd();
    } else if (!strcmp(argv[1], "pidfd-child")) {
        pidfd_child(argv);
        return 0;
    } else if (!strcmp(argv[1], "fds-child")) {
        fds_child(argv);
        return 0;
    } else {
        CHECK(0);
    }
    printf("moved %s ok\n", argv[1]);
    return 0;
}
