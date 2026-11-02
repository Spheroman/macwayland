// A private file mapping sees later changes to the file in the pages it
// hasn't written, as on Linux. Wine relies on it: wineserver writes the
// session object (window classes, desktops) into an unlinked temp file, and
// every client maps it MAP_PRIVATE and read-only, expecting to see objects
// the server adds after the mapping was made (dlls/ntdll/unix/virtual.c,
// map_file_into_view). Here a forked "server" writes through MAP_SHARED and
// pwrite while this process watches through MAP_PRIVATE.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(c) do { if (!(c)) { fprintf(stderr, "failed: %s (line %d, errno %d)\n", #c, __LINE__, errno); exit(1); } } while (0)
#define LEN (64 << 10)

int main(void) {
    char name[] = "/tmp/macwl-privmap-XXXXXX";
    int fd = mkstemp(name);
    CHECK(fd >= 0);
    unlink(name);
    CHECK(ftruncate(fd, LEN) == 0);

    // Wine's sequence: map read-write, then drop to read-only.
    char *priv = mmap(NULL, LEN, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    CHECK(priv != MAP_FAILED);
    CHECK(mprotect(priv, LEN, PROT_READ) == 0);
    // Touch every page first, so later changes can't come from a late fill.
    for (int i = 0; i < LEN; i += 4096) CHECK(priv[i] == 0);

    int go[2], done[2];
    CHECK(pipe(go) == 0 && pipe(done) == 0);
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        char *shared = mmap(NULL, LEN, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        CHECK(shared != MAP_FAILED);
        char c;
        CHECK(read(go[0], &c, 1) == 1);
        strcpy(shared + 0x5010, "from the server");
        CHECK(pwrite(fd, "by pwrite", 9, 0xa000) == 9);
        CHECK(write(done[1], "x", 1) == 1);
        CHECK(read(go[0], &c, 1) == 1);
        strcpy(shared + 0x3000, "after the copy");
        strcpy(shared + 0x4000, "next door");
        CHECK(write(done[1], "x", 1) == 1);
        _exit(0);
    }
    char c;
    CHECK(write(go[1], "x", 1) == 1);
    CHECK(read(done[0], &c, 1) == 1);
    CHECK(strcmp(priv + 0x5010, "from the server") == 0);
    CHECK(memcmp(priv + 0xa000, "by pwrite", 9) == 0);

    // A page this process writes becomes its own copy; its neighbours keep
    // following the file.
    CHECK(mprotect(priv, LEN, PROT_READ | PROT_WRITE) == 0);
    strcpy(priv + 0x3000, "mine");
    CHECK(write(go[1], "x", 1) == 1);
    CHECK(read(done[0], &c, 1) == 1);
    CHECK(strcmp(priv + 0x3000, "mine") == 0);
    CHECK(strcmp(priv + 0x4000, "next door") == 0);
    // A system call writing into the mapping makes a copy too.
    CHECK(pread(fd, priv + 0x6000, 9, 0x4000) == 9);
    CHECK(memcmp(priv + 0x6000, "next door", 9) == 0);
    char check[16];
    CHECK(pread(fd, check, 9, 0x6000) == 9 && check[0] == 0);
    int status;
    CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);

    // A forked child gets this process's copies, and the file's pages.
    pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        CHECK(strcmp(priv + 0x3000, "mine") == 0);
        CHECK(strcmp(priv + 0x5010, "from the server") == 0);
        _exit(0);
    }
    CHECK(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    puts("privmap ok");
    return 0;
}
