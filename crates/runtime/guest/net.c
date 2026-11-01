// A Wayland client's data path, without the protocol. argv[1] is the path of
// a host Unix socket (a stand-in compositor). Connect, put a frame in a
// memfd-backed shared buffer, send the fd with SCM_RIGHTS, wait for "ok",
// then check the compositor's write into the same buffer.
#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s\n", __LINE__, #cond); return 1; } } while (0)

int main(int argc, char **argv) {
    CHECK(argc == 2);
    int s = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    CHECK(s >= 0);
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    strncpy(addr.sun_path, argv[1], sizeof addr.sun_path - 1);
    CHECK(connect(s, (struct sockaddr *)&addr, sizeof addr) == 0);

    int fd = memfd_create("wl_shm", MFD_CLOEXEC);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, 4096) == 0);
    char *px = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(px != MAP_FAILED);
    strcpy(px, "frame 1");

    char data[] = "pool";
    struct iovec iov = {data, 4};
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    struct msghdr msg = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = control.buf, .msg_controllen = sizeof control.buf};
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof fd);
    CHECK(sendmsg(s, &msg, MSG_NOSIGNAL) == 4);

    char reply[2];
    CHECK(read(s, reply, 2) == 2 && memcmp(reply, "ok", 2) == 0);
    CHECK(strcmp(px + 100, "seen by host") == 0);
    puts("net ok");
    return 0;
}
