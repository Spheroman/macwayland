// signalfd and close_range: see the `signalfd` test in crates/runtime/tests/run.rs.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/epoll.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "failed: %s (line %d, errno %d)\n", #c, __LINE__, errno); exit(1); } } while (0)

static int close_range_(unsigned first, unsigned last, unsigned flags) {
    return syscall(SYS_close_range, first, last, flags);
}

// Read signals from `fd` until those in `want` have all come (or 5 s pass).
static void read_until(int fd, sigset_t *want) {
    int ep = epoll_create1(0);
    struct epoll_event ev = { .events = EPOLLIN };
    CHECK(epoll_ctl(ep, EPOLL_CTL_ADD, fd, &ev) == 0);
    for (int tries = 0; tries < 50 && !sigisemptyset(want); tries++) {
        if (epoll_wait(ep, &ev, 1, 100) != 1)
            continue;
        struct signalfd_siginfo si;
        ssize_t n;
        while ((n = read(fd, &si, sizeof si)) == sizeof si)
            sigdelset(want, si.ssi_signo);
        CHECK(n < 0 && errno == EAGAIN);
    }
    CHECK(sigisemptyset(want));
    close(ep);
}

int main(void) {
    // close_range: closed, then marked close-on-exec.
    for (int fd = 10; fd < 16; fd++)
        CHECK(dup2(1, fd) == fd);
    CHECK(close_range_(10, 12, 0) == 0);
    CHECK(fcntl(10, F_GETFD) < 0 && errno == EBADF);
    CHECK(fcntl(12, F_GETFD) < 0 && errno == EBADF);
    CHECK(fcntl(13, F_GETFD) == 0);
    CHECK(close_range_(13, ~0U, 4 /* CLOSE_RANGE_CLOEXEC */) == 0);
    CHECK(fcntl(15, F_GETFD) == FD_CLOEXEC);
    CHECK(close_range_(13, ~0U, 0) == 0);
    CHECK(close_range_(5, 4, 0) < 0 && errno == EINVAL);

    // Signals blocked, and read instead.
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    sigaddset(&mask, SIGTERM);
    sigaddset(&mask, SIGCHLD);
    CHECK(sigprocmask(SIG_BLOCK, &mask, NULL) == 0);
    int fd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
    CHECK(fd >= 0);
    struct signalfd_siginfo si;
    CHECK(read(fd, &si, sizeof si) < 0 && errno == EAGAIN);

    // One the process sends itself.
    CHECK(kill(getpid(), SIGUSR1) == 0);
    CHECK(read(fd, &si, sizeof si) == sizeof si);
    CHECK(si.ssi_signo == SIGUSR1 && si.ssi_pid == (unsigned)getpid());
    CHECK(read(fd, &si, sizeof si) < 0 && errno == EAGAIN);

    // From another process (host signals): SIGTERM, whose default would kill
    // us, and the child's SIGCHLD.
    pid_t child = fork();
    if (child == 0) {
        kill(getppid(), SIGTERM);
        _exit(0);
    }
    sigset_t want;
    sigemptyset(&want);
    sigaddset(&want, SIGTERM);
    sigaddset(&want, SIGCHLD);
    read_until(fd, &want);
    CHECK(waitpid(child, NULL, 0) == child);

    // A blocking read, woken by a signal that comes while it waits.
    sigset_t usr2;
    sigemptyset(&usr2);
    sigaddset(&usr2, SIGUSR2);
    CHECK(sigprocmask(SIG_BLOCK, &usr2, NULL) == 0);
    int blocking = signalfd(-1, &usr2, 0);
    CHECK(blocking >= 0);
    child = fork();
    if (child == 0) {
        usleep(200000);
        kill(getppid(), SIGUSR2);
        _exit(0);
    }
    CHECK(read(blocking, &si, sizeof si) == sizeof si);
    CHECK(si.ssi_signo == SIGUSR2);
    CHECK(waitpid(child, NULL, 0) == child);

    printf("signalfd ok\n");
    return 0;
}
