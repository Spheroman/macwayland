// Sockets passed in SCM_RIGHTS while the socket carrying them is itself in
// flight (as Firefox hands a child its IPC socket through its fork server),
// and the end of the file once they arrive: see the in_flight module in
// crates/linux-abi/src/syscall/net.rs. Also a directory fd passed the
// same way, used with fchdir and as a dirfd, as wineserver's clients do.
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "failed: %s (line %d, errno %d)\n", #c, __LINE__, errno); exit(1); } } while (0)

static void send_fd(int s, int fd) {
    char b = 'x';
    struct iovec io = { &b, 1 };
    union { char buf[CMSG_SPACE(4)]; struct cmsghdr align; } c;
    memset(&c, 0, sizeof c);
    struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = c.buf, .msg_controllen = sizeof c.buf };
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    h->cmsg_level = SOL_SOCKET;
    h->cmsg_type = SCM_RIGHTS;
    h->cmsg_len = CMSG_LEN(4);
    memcpy(CMSG_DATA(h), &fd, 4);
    CHECK(sendmsg(s, &m, 0) == 1);
}

// Exactly one fd: the receipt the runtime adds never shows.
static int recv_fd(int s) {
    char b;
    struct iovec io = { &b, 1 };
    union { char buf[CMSG_SPACE(16)]; struct cmsghdr align; } c;
    struct msghdr m = { .msg_iov = &io, .msg_iovlen = 1, .msg_control = c.buf, .msg_controllen = sizeof c.buf };
    CHECK(recvmsg(s, &m, 0) == 1);
    struct cmsghdr *h = CMSG_FIRSTHDR(&m);
    CHECK(h && h->cmsg_len == CMSG_LEN(4) && !CMSG_NXTHDR(&m, h));
    int fd;
    memcpy(&fd, CMSG_DATA(h), 4);
    return fd;
}

int main(void) {
    int ctl[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, ctl) == 0);
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        // Slow to pick up the first socket, so both are in flight a while.
        close(ctl[0]);
        struct timespec ts = { 0, 200000000 };
        nanosleep(&ts, NULL);
        int info = recv_fd(ctl[1]);
        int ipc = recv_fd(info);
        CHECK(write(ipc, "child", 5) == 5);
        char buf[16];
        CHECK(read(ipc, buf, sizeof buf) == 6 && memcmp(buf, "parent", 6) == 0);
        _exit(0);
    }
    close(ctl[1]);
    int info[2], ipc[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, info) == 0);
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, ipc) == 0);
    send_fd(ctl[0], info[1]);
    close(info[1]);
    send_fd(info[0], ipc[1]);
    close(ipc[1]);
    CHECK(write(ipc[0], "parent", 6) == 6);
    // A socket closed with fds in flight: the host collects garbage.
    for (int i = 0; i < 4; i++) {
        int x[2], y[2];
        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, x) == 0 && socketpair(AF_UNIX, SOCK_STREAM, 0, y) == 0);
        send_fd(x[0], y[0]);
        close(x[0]); close(x[1]); close(y[0]); close(y[1]);
    }
    char buf[16];
    CHECK(read(ipc[0], buf, sizeof buf) == 5 && memcmp(buf, "child", 5) == 0);

    // The child exits: with nothing left holding its end, the parent sees
    // the end of the file (how Firefox learns a child died).
    int st;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    struct pollfd p = { ipc[0], POLLIN, 0 };
    CHECK(poll(&p, 1, 2000) == 1);
    CHECK(read(ipc[0], buf, sizeof buf) == 0);

    // A directory opened by another process: list it after fchdir, and
    // open a file in it.
    char dir[] = "/tmp/macwl-passfd-XXXXXX";
    CHECK(mkdtemp(dir));
    char file[64];
    snprintf(file, sizeof file, "%s/inside", dir);
    int f = open(file, O_CREAT | O_WRONLY, 0600);
    CHECK(f >= 0);
    close(f);
    int pair[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, pair) == 0);
    pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        int d = open(dir, O_RDONLY | O_DIRECTORY);
        CHECK(d >= 0);
        send_fd(pair[1], d);
        _exit(0);
    }
    int d = recv_fd(pair[0]);
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    int in = openat(d, "inside", O_RDONLY);
    CHECK(in >= 0);
    close(in);
    int cwd = open(".", O_RDONLY);
    CHECK(fchdir(d) == 0);
    DIR *list = opendir(".");
    CHECK(list);
    int found = 0;
    struct dirent *e;
    while ((e = readdir(list))) found |= !strcmp(e->d_name, "inside");
    closedir(list);
    CHECK(found);
    CHECK(fchdir(cwd) == 0);
    CHECK(unlink(file) == 0 && rmdir(dir) == 0);
    puts("passfd ok");
    return 0;
}
