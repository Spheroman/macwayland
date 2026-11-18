// A Unix socket at a path too long for macOS's sun_path (104 bytes) but
// within Linux's (108): argv[1] is a directory, and the socket is
// argv[1]/NAME, padded so the whole path is 105–107 bytes. Bind, listen,
// connect, pass a message both ways, and check getsockname, getpeername
// and accept say the path as given. Prints "longsock ok".
#define _GNU_SOURCE
#include <errno.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { printf("FAIL line %d: %s (errno %d)\n", __LINE__, #c, errno); return 1; } } while (0)

static int same_path(const struct sockaddr_un *a, socklen_t len, const char *path) {
    return len == offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1 && a->sun_family == AF_UNIX && !strcmp(a->sun_path, path);
}

int main(int argc, char **argv) {
    CHECK(argc == 2);
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    // The name fills the path to 106 bytes.
    int dir = strlen(argv[1]);
    CHECK(dir + 2 < 106);
    int n = snprintf(addr.sun_path, sizeof addr.sun_path, "%s/", argv[1]);
    memset(addr.sun_path + n, 's', 106 - n);
    addr.sun_path[106] = 0;
    const char *path = addr.sun_path;
    CHECK(strlen(path) == 106);
    socklen_t len = sizeof addr;

    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(l >= 0);
    CHECK(bind(l, (struct sockaddr *)&addr, len) == 0);
    CHECK(listen(l, 1) == 0);
    CHECK(access(path, F_OK) == 0);

    struct sockaddr_un got;
    socklen_t got_len = sizeof got;
    CHECK(getsockname(l, (struct sockaddr *)&got, &got_len) == 0);
    CHECK(same_path(&got, got_len, path));

    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(c >= 0);
    CHECK(connect(c, (struct sockaddr *)&addr, len) == 0);
    got_len = sizeof got;
    CHECK(getpeername(c, (struct sockaddr *)&got, &got_len) == 0);
    CHECK(same_path(&got, got_len, path));

    int a = accept(l, NULL, NULL);
    CHECK(a >= 0);
    char buf[8];
    CHECK(write(c, "ping", 4) == 4);
    CHECK(read(a, buf, sizeof buf) == 4 && !memcmp(buf, "ping", 4));
    CHECK(write(a, "pong", 4) == 4);
    CHECK(read(c, buf, sizeof buf) == 4 && !memcmp(buf, "pong", 4));

    // Its file goes as any other.
    CHECK(unlink(path) == 0);
    CHECK(access(path, F_OK) < 0 && errno == ENOENT);
    printf("longsock ok\n");
    return 0;
}
