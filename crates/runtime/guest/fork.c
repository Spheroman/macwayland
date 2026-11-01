// Processes: fork, copy-on-write memory, shared memory, inherited fds, pipes,
// wait statuses, execve, posix_spawn (vfork), and forking from a forked child.
// Run as `fork`; it re-execs itself as `fork exec-child N` and so on.
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s\n", __LINE__, #cond); fflush(stdout); _exit(1); } } while (0)

extern char **environ;
static volatile long cow = 1;
static char self_path[4096];

static int wait_for(pid_t pid) {
    int st;
    CHECK(waitpid(pid, &st, 0) == pid);
    return st;
}

static void copy_on_write(void) {
    int to_child[2], to_parent[2];
    CHECK(pipe(to_child) == 0 && pipe(to_parent) == 0);
    pid_t parent = getpid();
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        CHECK(getppid() == parent);
        CHECK(getpid() != parent);
        CHECK(cow == 1);
        cow = 2; // the parent mustn't see this
        char c;
        CHECK(read(to_child[0], &c, 1) == 1); // the parent has written 3 by now
        CHECK(cow == 2);
        CHECK(write(to_parent[1], "k", 1) == 1);
        _exit(42);
    }
    cow = 3; // nor must the child see this
    CHECK(write(to_child[1], "go", 1) == 1);
    char c;
    CHECK(read(to_parent[0], &c, 1) == 1 && c == 'k');
    int st = wait_for(pid);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 42);
    CHECK(cow == 3);
    close(to_child[0]), close(to_child[1]), close(to_parent[0]), close(to_parent[1]);
}

static void shared_memory_and_fds(void) {
    volatile int *shared = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(shared != MAP_FAILED);
    char path[] = "/tmp/macwl-fork-XXXXXX";
    int fd = mkstemp(path);
    CHECK(fd >= 0);
    unlink(path);
    CHECK(write(fd, "abcdef", 6) == 6);
    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    pid_t pid = fork();
    if (pid == 0) {
        *shared = 7;
        char buf[3];
        CHECK(read(fd, buf, 3) == 3 && memcmp(buf, "abc", 3) == 0);
        _exit(0);
    }
    CHECK(WEXITSTATUS(wait_for(pid)) == 0);
    CHECK(*shared == 7);                   // MAP_SHARED is shared
    CHECK(lseek(fd, 0, SEEK_CUR) == 3);    // and so is the file offset
    close(fd);
}

static void signals_in_wait_status(void) {
    pid_t pid = fork();
    if (pid == 0) abort();
    int st = wait_for(pid);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGABRT);
    pid = fork();
    if (pid == 0) pause();
    CHECK(kill(pid, SIGTERM) == 0);
    st = wait_for(pid);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM);
    // A child's own SIGUSR1 (10 on Linux, 30 on macOS) comes back as 10.
    pid = fork();
    if (pid == 0) pause();
    CHECK(kill(pid, SIGUSR1) == 0);
    st = wait_for(pid);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGUSR1);
    CHECK(waitpid(-1, &st, WNOHANG) == -1); // no children left
}

static void exec_and_spawn(void) {
    int out[2];
    CHECK(pipe(out) == 0);
    pid_t pid = fork();
    if (pid == 0) {
        dup2(out[1], 1);
        char *argv[] = {"fork", "exec-child", "5", 0};
        execve(self_path, argv, environ);
        _exit(99);
    }
    close(out[1]);
    char buf[64] = {0};
    CHECK(read(out[0], buf, sizeof buf - 1) > 0);
    CHECK(strcmp(buf, "exec'd with 5\n") == 0);
    CHECK(WEXITSTATUS(wait_for(pid)) == 5);
    close(out[0]);

    // posix_spawn: clone(CLONE_VM | CLONE_VFORK) under the hood.
    char *argv[] = {"fork", "exec-child", "6", 0};
    CHECK(posix_spawn(&pid, self_path, 0, 0, argv, environ) == 0);
    CHECK(WEXITSTATUS(wait_for(pid)) == 6);
    char *missing[] = {"nope", 0};
    int r = posix_spawn(&pid, "/nonexistent/program", 0, 0, missing, environ);
    CHECK(r == ENOENT || WEXITSTATUS(wait_for(pid)) == 127);
}

// How shells wait for a background job: a SIGCHLD handler, SIGCHLD blocked,
// poll with WNOHANG, and sigsuspend until the handler has run. (With SIGCHLD
// at its default, ignored, sigsuspend wouldn't wake: as on Linux.)
static void on_chld(int sig) { (void)sig; }

static void suspend_until_child_exits(void) {
    signal(SIGCHLD, on_chld);
    sigset_t chld, old;
    sigemptyset(&chld);
    sigaddset(&chld, SIGCHLD);
    sigprocmask(SIG_BLOCK, &chld, &old);
    pid_t pid = fork();
    if (pid == 0) {
        usleep(50000);
        _exit(8);
    }
    int st, rounds = 0;
    while (waitpid(pid, &st, WNOHANG) == 0) {
        CHECK(++rounds < 100); // asleep in sigsuspend, not spinning
        CHECK(sigsuspend(&old) == -1 && errno == EINTR);
    }
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 8);
    sigprocmask(SIG_SETMASK, &old, 0);
    signal(SIGCHLD, SIG_DFL);
}

static void nested(void) {
    pid_t pid = fork();
    if (pid == 0) {
        cow = 10;
        pid_t grandchild = fork();
        if (grandchild == 0) _exit(cow == 10 ? 3 : 4);
        _exit(WEXITSTATUS(wait_for(grandchild)));
    }
    CHECK(WEXITSTATUS(wait_for(pid)) == 3);
}

int main(int argc, char **argv) {
    if (argc == 3 && strcmp(argv[1], "exec-child") == 0) {
        printf("exec'd with %s\n", argv[2]);
        return atoi(argv[2]);
    }
    CHECK(argc == 2);
    strncpy(self_path, argv[1], sizeof self_path - 1);
    copy_on_write();
    shared_memory_and_fds();
    signals_in_wait_status();
    exec_and_spawn();
    suspend_until_child_exits();
    nested();
    puts("fork ok");
    return 0;
}
