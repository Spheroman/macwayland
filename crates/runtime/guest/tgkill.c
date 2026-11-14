// tgkill from one process to a thread of another, as wineserver suspends
// and ends its clients' threads: never ESRCH while the thread lives, and the
// signal arrives.
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); fflush(stdout); _exit(1); } } while (0)

static volatile sig_atomic_t got;
static volatile long thread_tid;
static int ready[2], done[2];

static void on_usr1(int sig) {
    (void)sig;
    got = 1;
}

static void *thread_main(void *arg) {
    (void)arg;
    thread_tid = syscall(SYS_gettid);
    CHECK(write(ready[1], "r", 1) == 1);
    // Alive, in a read, until the test is over (a signal may interrupt it).
    char c;
    while (read(done[0], &c, 1) < 0 && errno == EINTR) {
    }
    return 0;
}

int main(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    CHECK(sigaction(SIGUSR1, &sa, 0) == 0);
    CHECK(pipe(ready) == 0 && pipe(done) == 0);
    pthread_t th;
    CHECK(pthread_create(&th, 0, thread_main, 0) == 0);
    char c;
    CHECK(read(ready[0], &c, 1) == 1);
    pid_t parent = getpid();
    long tid = thread_tid;
    CHECK(tid > 0 && tid != parent);

    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        // The parent's thread, from another process: there, so no ESRCH.
        CHECK(syscall(SYS_tgkill, parent, tid, 0) == 0);
        CHECK(syscall(SYS_tgkill, parent, parent, 0) == 0);
        CHECK(syscall(SYS_tgkill, parent, tid, SIGUSR1) == 0);
        CHECK(syscall(SYS_tgkill, 0, tid, 0) == -1 && errno == EINVAL);
        _exit(0);
    }
    int st;
    pid_t w;
    while ((w = waitpid(pid, &st, 0)) < 0 && errno == EINTR) {
    }
    CHECK(w == pid && WIFEXITED(st) && WEXITSTATUS(st) == 0);
    // The signal came (in 1:1 mode to the process: macOS can't aim at
    // another process's thread).
    for (int i = 0; i < 500 && !got; i++) {
        usleep(10000);
    }
    CHECK(got);
    CHECK(write(done[1], "d", 1) == 1);
    CHECK(pthread_join(th, 0) == 0);
    printf("tgkill ok\n");
    return 0;
}
