// Process churn: hundreds of processes that end every way a Linux process
// can, none of them waited for. Each guest process is a Mac process with a
// VM, so every one must be gone once its Linux process has ended.
//
// Run as `procstress SELF [ROUNDS]`. Every process it makes writes
// "pid N" to stdout as it starts, so the host can check that it's gone. At
// the end it writes "check" and waits for stdin to close (the host looks
// while the zombies' parent is still alive), then "procstress ok".
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { dprintf(2, "FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); _exit(1); } } while (0)

static char *self_path;

static void announce(void) {
    char line[32];
    int n = snprintf(line, sizeof line, "pid %d\n", getpid());
    CHECK(write(1, line, n) == n); // one write: atomic on a pipe
}

static void sleep_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000 };
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {}
}

static int never_written[2];

// Blocks in a host call that never returns on its own.
static void *block_in_read(void *arg) {
    (void)arg;
    char c;
    read(never_written[0], &c, 1);
    return 0;
}

static void *exit_group_from_thread(void *arg) {
    sleep_ms(2);
    syscall(SYS_exit_group, (long)arg);
    return 0;
}

static _Noreturn void *sleeper(void *arg) {
    (void)arg;
    for (;;) sleep_ms(1000);
}

// The children: each announces itself, then ends one way.
enum { EXIT, EXIT_GROUP_FROM_THREAD, EXIT_WITH_BLOCKED_THREAD, KILLED, TERMINATED, EXEC, DAEMON, FORK_THEN_EXIT, FORK_THEN_EXEC, FORKED_THEN_KILLED, KINDS };

static void child(int kind, int parent_pipe) {
    announce();
    pthread_t t;
    switch (kind) {
    case EXIT:
        _exit(0);
    case EXIT_GROUP_FROM_THREAD:
        // The main thread is blocked in read() when another ends the process.
        CHECK(pthread_create(&t, 0, block_in_read, 0) == 0);
        CHECK(pthread_create(&t, 0, exit_group_from_thread, (void *)3) == 0);
        block_in_read(0);
        _exit(1);
    case EXIT_WITH_BLOCKED_THREAD:
        CHECK(pthread_create(&t, 0, block_in_read, 0) == 0);
        CHECK(pthread_create(&t, 0, sleeper, 0) == 0);
        sleep_ms(2);
        exit(0);
    case FORKED_THEN_KILLED: {
        // Killed with a spare waiting (it has forked) and a live child.
        pid_t pid = fork();
        CHECK(pid >= 0);
        if (pid == 0) {
            announce();
            sleep_ms(20);
            _exit(0);
        }
    }
    // fall through
    case KILLED:
    case TERMINATED:
        // Threads too, so a signal has several vCPUs to stop.
        CHECK(pthread_create(&t, 0, block_in_read, 0) == 0);
        CHECK(pthread_create(&t, 0, sleeper, 0) == 0);
        CHECK(write(parent_pipe, "r", 1) == 1);
        for (;;) pause();
    case EXEC: {
        char *argv[] = { self_path, "exit", 0 };
        execv(self_path, argv);
        _exit(1);
    }
    case DAEMON: {
        // As a daemon (wineserver) starts: fork, setsid, fork again, the
        // middle one exits, and the grandchild runs on, parented by launchd.
        pid_t pid = fork();
        CHECK(pid >= 0);
        if (pid == 0) {
            CHECK(setsid() > 0);
            pid_t pid2 = fork();
            CHECK(pid2 >= 0);
            if (pid2 == 0) {
                announce();
                sleep_ms(50);
                _exit(0);
            }
            _exit(0);
        }
        int st;
        CHECK(waitpid(pid, &st, 0) == pid);
        _exit(0);
    }
    case FORK_THEN_EXIT: {
        // Having forked, it has a spare; its child outlives it.
        pid_t pid = fork();
        CHECK(pid >= 0);
        if (pid == 0) {
            announce();
            sleep_ms(20);
            _exit(0);
        }
        _exit(0);
    }
    case FORK_THEN_EXEC: {
        pid_t pid = fork();
        CHECK(pid >= 0);
        if (pid == 0) {
            announce();
            _exit(0);
        }
        char *argv[] = { self_path, "exit", 0 };
        execv(self_path, argv);
        _exit(1);
    }
    }
    _exit(1);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "exit") == 0) {
        sleep_ms(1);
        return 0;
    }
    CHECK(argc > 1);
    self_path = argv[1];
    int rounds = argc > 2 ? atoi(argv[2]) : 300;
    CHECK(pipe(never_written) == 0);
    int ready[2];
    CHECK(pipe(ready) == 0);
    // Children are never waited for; ignoring SIGCHLD would reap them, so
    // leave it at its default and let them be zombies.
    for (int i = 0; i < rounds; i++) {
        int kind = i % KINDS;
        pid_t pid = fork();
        CHECK(pid >= 0);
        if (pid == 0) child(kind, ready[1]);
        if (kind == KILLED || kind == TERMINATED || kind == FORKED_THEN_KILLED) {
            char c;
            CHECK(read(ready[0], &c, 1) == 1);
            CHECK(kill(pid, kind == TERMINATED ? SIGTERM : SIGKILL) == 0);
        }
        // Keep a few dozen alive at once, not hundreds.
        if (i % 16 == 15) sleep_ms(30);
    }
    sleep_ms(200);
    CHECK(write(1, "check\n", 6) == 6);
    char c;
    while (read(0, &c, 1) > 0) {}
    CHECK(write(1, "procstress ok\n", 14) == 14);
    return 0;
}
