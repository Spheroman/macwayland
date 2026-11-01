// Shared-mapping checks. The test passes an 8 KiB host file as fd 3: the
// first page is 'h', the second '.'. `shm` alone maps it MAP_SHARED, reads the
// host's data, writes the second page (the test checks the file), and checks
// shared anonymous memory. `shm eof` touches a mapped page wholly past the end
// of the file, which must fault (SIGBUS on Linux). A shared, read-only
// mapping of a file opened read-only (fontconfig's caches) works too.
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s\n", __LINE__, #cond); return 1; } } while (0)

int main(int argc, char **argv) {
    // `shm private`: fd 3 is a host POSIX shm object (another program's),
    // mapped privately as clients map a compositor's keymap.
    if (argc > 1 && strcmp(argv[1], "private") == 0) {
        char *p = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, 3, 0);
        CHECK(p != MAP_FAILED);
        CHECK(p[0] == 'k' && p[4095] == 'k');
        puts("private ok");
        return 0;
    }
    if (argc > 1 && strcmp(argv[1], "eof") == 0) {
        volatile char *p = mmap(NULL, 64 << 10, PROT_READ, MAP_SHARED, 3, 0);
        CHECK(p != MAP_FAILED);
        CHECK(p[0] == 'h');
        (void)p[32 << 10]; // must fault
        puts("BUG: access past EOF did not fault");
        return 1;
    }

    char *p = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, 3, 0);
    CHECK(p != MAP_FAILED);
    CHECK(p[0] == 'h' && p[4095] == 'h' && p[4096] == '.');
    memset(p + 4096, 'g', 4096);

    // A second mapping at a 4 KiB offset sees the first one's writes.
    char *q = mmap(NULL, 4096, PROT_READ, MAP_SHARED, 3, 4096);
    CHECK(q != MAP_FAILED);
    CHECK(q[0] == 'g' && q[4095] == 'g');
    CHECK(munmap(q, 4096) == 0);
    CHECK(munmap(p, 8192) == 0);

    // Read-only, through a read-only fd: sees the file, writes and all.
    int ro = open("/proc/self/fd/3", O_RDONLY);
    CHECK(ro >= 0);
    char *r = mmap(NULL, 8192, PROT_READ, MAP_SHARED, ro, 0);
    CHECK(r != MAP_FAILED);
    CHECK(r[0] == 'h' && r[4096] == 'g');

    unsigned char *a = mmap(NULL, 3 << 20, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(a != MAP_FAILED);
    CHECK(a[(3 << 20) - 1] == 0);
    memset(a, 7, 3 << 20);
    CHECK(a[(3 << 20) - 1] == 7);
    puts("shm ok");
    return 0;
}
