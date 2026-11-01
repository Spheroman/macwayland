// Memory-management checks for the runtime. `mm` alone exercises malloc,
// mmap, mprotect and munmap and exits 0. `mm mprotect` and `mm munmap` make
// an access that must fault; if it doesn't (say, a stale TLB entry), they
// print BUG and exit 1.
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s\n", __LINE__, #cond); return 1; } } while (0)

static int basics(void) {
    // malloc: small sizes come from brk, large ones from mmap.
    void *ptrs[256];
    for (int i = 0; i < 256; i++) {
        size_t n = (size_t)(i + 1) * 4099;
        ptrs[i] = malloc(n);
        CHECK(ptrs[i]);
        memset(ptrs[i], i, n);
    }
    for (int i = 0; i < 256; i++) {
        CHECK(((unsigned char *)ptrs[i])[(size_t)i * 4099] == (unsigned char)i);
        free(ptrs[i]);
    }

    // A big reservation, partly committed: only touched pages cost memory.
    size_t gib = (size_t)1 << 30;
    unsigned char *r = mmap(NULL, gib, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
    CHECK(r != MAP_FAILED);
    CHECK(mprotect(r + (512 << 20), 16 << 20, PROT_READ | PROT_WRITE) == 0);
    memset(r + (512 << 20), 0x5a, 16 << 20);
    CHECK(r[(512 << 20) + (16 << 20) - 1] == 0x5a);

    // Punch a hole, then map it again: fresh pages read as zero.
    CHECK(munmap(r + (520 << 20), 1 << 20) == 0);
    CHECK(mmap(r + (520 << 20), 1 << 20, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == r + (520 << 20));
    CHECK(r[520 << 20] == 0);
    CHECK(r[(520 << 20) - 1] == 0x5a);
    CHECK(munmap(r, gib) == 0);

    // The same page mapped, touched and unmapped over and over (as musl's
    // stdio does with FILE buffers): each first touch faults, legitimately.
    for (int i = 0; i < 10; i++) {
        volatile char *q = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(q != MAP_FAILED && q[0] == 0);
        q[0] = 1;
        CHECK(munmap((void *)q, 4096) == 0);
    }

    // Errors.
    CHECK(mprotect(r, 4096, PROT_READ) == -1);
    CHECK(munmap((char *)r + 1, 4096) == -1);
    puts("mm ok");
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2)
        return basics();

    volatile unsigned char *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED);
    p[0] = 1; // now mapped and in the TLB
    if (strcmp(argv[1], "mprotect") == 0) {
        CHECK(mprotect((void *)p, 4096, PROT_READ) == 0);
        CHECK(p[0] == 1);
        p[0] = 2; // must fault
    } else if (strcmp(argv[1], "munmap") == 0) {
        CHECK(munmap((void *)p, 4096) == 0);
        (void)p[0]; // must fault
    }
    puts("BUG: access did not fault");
    return 1;
}
