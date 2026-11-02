// What a process costs to make: fork + exit + wait, fork + exec + exit +
// wait, and posix_spawn (vfork + exec), from a process with MB of private
// memory touched (to see how fork's cost grows with the address space).
//
// Run as `forkbench MODE N MB [PROGRAM]`, MODE one of fork, exec, spawn.
// PROGRAM (exec and spawn) is run with no arguments and should exit at once
// (forkbench itself, given no arguments, does). Prints the first round's
// time and the mean of the rest, in microseconds.
#define _GNU_SOURCE
#include <errno.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

#define CHECK(cond) do { if (!(cond)) { dprintf(2, "FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); _exit(1); } } while (0)

static double now_us(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1e6 + ts.tv_nsec / 1e3;
}

int main(int argc, char **argv) {
    if (argc < 4) return 0; // as a program to exec: exit at once
    const char *mode = argv[1];
    int n = atoi(argv[2]);
    long mb = atol(argv[3]);
    char *prog = argc > 4 ? argv[4] : argv[0];
    if (mb > 0) {
        size_t len = (size_t)mb << 20;
        char *p = mmap(0, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        CHECK(p != MAP_FAILED);
        for (size_t i = 0; i < len; i += 4096) p[i] = (char)i | 1;
    }
    double first = 0, rest = 0;
    for (int i = 0; i < n; i++) {
        double t0 = now_us();
        pid_t pid;
        if (!strcmp(mode, "spawn")) {
            char *args[] = { prog, 0 };
            CHECK(posix_spawn(&pid, prog, 0, 0, args, environ) == 0);
        } else {
            pid = fork();
            CHECK(pid >= 0);
            if (pid == 0) {
                if (!strcmp(mode, "exec")) {
                    char *args[] = { prog, 0 };
                    execve(prog, args, environ);
                    _exit(127);
                }
                _exit(0);
            }
        }
        int st;
        CHECK(waitpid(pid, &st, 0) == pid);
        CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
        double dt = now_us() - t0;
        if (i == 0) first = dt; else rest += dt;
    }
    printf("%s %ld MB: first %.0f us, mean of %d after %.0f us\n", mode, mb, first, n - 1, n > 1 ? rest / (n - 1) : 0);
    return 0;
}
