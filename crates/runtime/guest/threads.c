// Threads: mutexes and condition variables (futex), joins (CLEARTID),
// thread-local storage, thread ids, signals to one thread, a thread blocked
// in read() while others run, many threads at once, fork and exec from a
// threaded process, and exit() from a thread ending all of them.
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s\n", __LINE__, #cond); fflush(stdout); _exit(1); } } while (0)

static pid_t gettid_(void) { return syscall(SYS_gettid); }

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static long counter;
static __thread int tls_value;

static void *count(void *arg) {
    tls_value = (int)(long)arg;
    for (int i = 0; i < 20000; i++) {
        pthread_mutex_lock(&lock);
        counter++;
        pthread_mutex_unlock(&lock);
    }
    CHECK(tls_value == (int)(long)arg); // nobody else's
    return (void *)(long)gettid_();
}

static void mutexes_and_joins(void) {
    enum { N = 8 };
    pthread_t t[N];
    pid_t tids[N];
    for (long i = 0; i < N; i++) CHECK(pthread_create(&t[i], 0, count, (void *)i) == 0);
    for (int i = 0; i < N; i++) {
        void *r;
        CHECK(pthread_join(t[i], &r) == 0);
        tids[i] = (pid_t)(long)r;
        CHECK(tids[i] != getpid() && tids[i] > 0);
        for (int j = 0; j < i; j++) CHECK(tids[j] != tids[i]);
    }
    CHECK(counter == N * 20000);
    CHECK(gettid_() == getpid());
}

static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static int queue[16], head, tail;

static void *producer(void *arg) {
    (void)arg;
    for (int i = 1; i <= 1000; i++) {
        pthread_mutex_lock(&lock);
        while (tail - head == 16) pthread_cond_wait(&cond, &lock);
        queue[tail++ % 16] = i;
        pthread_cond_broadcast(&cond);
        pthread_mutex_unlock(&lock);
    }
    return 0;
}

static void condition_variables(void) {
    pthread_t p;
    CHECK(pthread_create(&p, 0, producer, 0) == 0);
    long sum = 0;
    for (int got = 0; got < 1000; got++) {
        pthread_mutex_lock(&lock);
        while (head == tail) pthread_cond_wait(&cond, &lock);
        sum += queue[head++ % 16];
        pthread_cond_broadcast(&cond);
        pthread_mutex_unlock(&lock);
    }
    pthread_join(p, 0);
    CHECK(sum == 1000 * 1001 / 2);
}

static volatile pid_t handled_on;
static void on_usr1(int sig) { (void)sig; handled_on = gettid_(); }
static volatile int worker_ready;
static volatile pid_t worker_tid;
static int pipe_fds[2];

static void *blocked_reader(void *arg) {
    (void)arg;
    worker_tid = gettid_();
    worker_ready = 1;
    char c;
    ssize_t n;
    // The signal interrupts the read (no SA_RESTART); then the data comes.
    while ((n = read(pipe_fds[0], &c, 1)) == -1 && errno == EINTR) {}
    CHECK(n == 1 && c == 'x');
    return 0;
}

static void signals_and_blocking(void) {
    struct sigaction sa = {0};
    sa.sa_handler = on_usr1;
    sigaction(SIGUSR1, &sa, 0);
    CHECK(pipe(pipe_fds) == 0);
    pthread_t t;
    CHECK(pthread_create(&t, 0, blocked_reader, 0) == 0);
    while (!worker_ready) usleep(1000);
    usleep(20000); // let it block in read
    CHECK(pthread_kill(t, SIGUSR1) == 0);
    while (!handled_on) usleep(1000);
    CHECK(handled_on == worker_tid); // that thread, not this one
    CHECK(write(pipe_fds[1], "x", 1) == 1);
    CHECK(pthread_join(t, 0) == 0);
}

static void *idle(void *arg) {
    (void)arg;
    usleep(20000);
    return 0;
}

static void many_threads(void) {
    enum { N = 48 };
    pthread_t t[N];
    for (int i = 0; i < N; i++) CHECK(pthread_create(&t[i], 0, idle, 0) == 0);
    for (int i = 0; i < N; i++) CHECK(pthread_join(t[i], 0) == 0);
}

static void *spin(void *arg) {
    (void)arg;
    for (;;) usleep(1000);
    return 0;
}

static void fork_and_exec_with_threads(const char *self) {
    pthread_t t;
    CHECK(pthread_create(&t, 0, spin, 0) == 0);
    pid_t pid = fork();
    if (pid == 0) {
        // Only the forking thread comes along; threads work in the child.
        pthread_t u;
        CHECK(pthread_create(&u, 0, idle, 0) == 0);
        CHECK(pthread_join(u, 0) == 0);
        _exit(9);
    }
    int st;
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st) && WEXITSTATUS(st) == 9);
    pid = fork();
    if (pid == 0) {
        pthread_t u;
        pthread_create(&u, 0, spin, 0);
        // execve with another thread running: it goes away first.
        char *argv[] = {"threads", "exit-from-thread", 0};
        execv(self, argv);
        _exit(98);
    }
    CHECK(waitpid(pid, &st, 0) == pid && WIFEXITED(st));
    CHECK(WEXITSTATUS(st) == 42);
}

static void *exit_whole_process(void *arg) {
    (void)arg;
    usleep(10000);
    exit(42); // exit_group: ends the main thread blocked in join, too
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "exit-from-thread") == 0) {
        pthread_t t;
        pthread_create(&t, 0, exit_whole_process, 0);
        pthread_join(t, 0);
        return 1; // not reached
    }
    CHECK(argc == 2);
    // Each phase, as it starts, on stderr: where a hang was.
    fputs("phase mutexes\n", stderr);
    mutexes_and_joins();
    fputs("phase condvars\n", stderr);
    condition_variables();
    fputs("phase signals\n", stderr);
    signals_and_blocking();
    fputs("phase many\n", stderr);
    many_threads();
    fputs("phase fork-exec\n", stderr);
    fork_and_exec_with_threads(argv[1]);
    puts("threads ok");
    return 0;
}
