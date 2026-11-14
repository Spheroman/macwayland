// Shared memory and futexes across processes (docs/processes.md, step 7).
//
// `shmfutex TEST [ARGS]`, printing "shmfutex TEST ok" at the end:
//   fork      process-shared mutex, condvar, semaphore and barrier between
//             a parent and three forked children (shared anonymous memory)
//   shm       the same between a process and an unrelated one (a program
//             exec'd afresh), through a POSIX shm object both shm_open;
//             `shm NAME` makes NAME and leaves the peer (`peer shm NAME`)
//             to someone else
//   memfd     the same through a memfd passed over a Unix socket
//   robust    robust mutexes (plain and priority-inheriting) whose holder
//             exits holding them: the next locker gets EOWNERDEAD, whether
//             it was waiting already or came later; `robust kill`: the
//             holder is killed with SIGKILL instead
//   pi        a priority-inheriting process-shared mutex contended by
//             three processes; trylock while another holds it
//   requeue   FUTEX_CMP_REQUEUE of waiters in other processes; FUTEX_WAKE
//             counts; a stale CMP_REQUEUE gets EAGAIN
//   wakeop    FUTEX_WAKE_OP waking waiters in two other processes
//   waitv     futex_waitv on three futexes woken from another process
//   windows   two processes see each other's writes through a file, POSIX
//             shm, a memfd and shared anonymous memory, each mapped afresh
//             by both; one unmapping leaves the other's view; a private
//             mapping's first write stays its own
//   library FILE N  N processes map FILE privately, read-only, and touch
//             every page; prints "N mapped" and waits for stdin to close
//
// Internal: `peer KIND ...` is the exec'd side of `shm` and `memfd`.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL %d line %d: %s (errno %d)\n", (int)getpid(), __LINE__, #cond, errno); fflush(stdout); _exit(1); } } while (0)

static char *self;

static void sleep_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000 };
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR) {}
}

static long futex(void *addr, int op, int val, long val2, void *addr2, int val3) {
    return syscall(SYS_futex, addr, op, val, val2, addr2, val3);
}

static void wait_ok(pid_t pid) {
    int st;
    CHECK(waitpid(pid, &st, 0) == pid);
    if (!WIFEXITED(st) || WEXITSTATUS(st) != 0) {
        printf("FAIL child %d status %#x\n", (int)pid, st);
        fflush(stdout);
        _exit(1);
    }
}

// ---- mutex, condvar, semaphore and barrier, among K participants ----

#define MAXK 4
#define ROUNDS 300
#define INCREMENTS 5000

struct sync {
    pthread_mutex_t m;
    pthread_cond_t c;
    pthread_barrier_t b;
    sem_t sem[MAXK];
    int k;
    int turn;
    long counter;
    char data[64];
};

static void sync_init(struct sync *s, int k) {
    memset(s, 0, sizeof *s);
    s->k = k;
    pthread_mutexattr_t ma;
    pthread_mutexattr_init(&ma);
    CHECK(pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED) == 0);
    CHECK(pthread_mutex_init(&s->m, &ma) == 0);
    pthread_condattr_t ca;
    pthread_condattr_init(&ca);
    CHECK(pthread_condattr_setpshared(&ca, PTHREAD_PROCESS_SHARED) == 0);
    CHECK(pthread_cond_init(&s->c, &ca) == 0);
    pthread_barrierattr_t ba;
    pthread_barrierattr_init(&ba);
    CHECK(pthread_barrierattr_setpshared(&ba, PTHREAD_PROCESS_SHARED) == 0);
    CHECK(pthread_barrier_init(&s->b, &ba, k) == 0);
    for (int i = 0; i < k; i++)
        CHECK(sem_init(&s->sem[i], 1, 0) == 0);
}

// Participant `me` of `s->k`: every one runs this, each in its process.
static void sync_part(struct sync *s, int me) {
    int k = s->k;
    // The mutex, contended.
    pthread_barrier_wait(&s->b);
    for (int i = 0; i < INCREMENTS; i++) {
        CHECK(pthread_mutex_lock(&s->m) == 0);
        s->counter++;
        CHECK(pthread_mutex_unlock(&s->m) == 0);
    }
    pthread_barrier_wait(&s->b);
    CHECK(s->counter == (long)k * INCREMENTS);
    // A condition variable passing a turn round the ring.
    for (int r = 0; r < ROUNDS; r++) {
        CHECK(pthread_mutex_lock(&s->m) == 0);
        while (s->turn != me)
            CHECK(pthread_cond_wait(&s->c, &s->m) == 0);
        s->turn = (me + 1) % k;
        CHECK(pthread_cond_broadcast(&s->c) == 0);
        CHECK(pthread_mutex_unlock(&s->m) == 0);
    }
    pthread_barrier_wait(&s->b);
    // Semaphores passing a token round the ring; 0 waits first for a bit,
    // so the others block in sem_wait.
    if (me == 0)
        sleep_ms(20);
    for (int r = 0; r < ROUNDS; r++) {
        if (me == 0) {
            CHECK(sem_post(&s->sem[1 % k]) == 0);
            while (sem_wait(&s->sem[0]) < 0)
                CHECK(errno == EINTR);
        } else {
            while (sem_wait(&s->sem[me]) < 0)
                CHECK(errno == EINTR);
            CHECK(sem_post(&s->sem[(me + 1) % k]) == 0);
        }
    }
    // The barrier, many times.
    for (int r = 0; r < ROUNDS; r++)
        pthread_barrier_wait(&s->b);
    int v;
    for (int i = 0; i < k; i++) {
        CHECK(sem_getvalue(&s->sem[i], &v) == 0);
        CHECK(v == 0);
    }
}

static void test_fork(void) {
    struct sync *s = mmap(NULL, sizeof *s, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(s != MAP_FAILED);
    sync_init(s, 4);
    pid_t kids[3];
    for (int i = 1; i < 4; i++) {
        kids[i - 1] = fork();
        CHECK(kids[i - 1] >= 0);
        if (kids[i - 1] == 0) {
            sync_part(s, i);
            _exit(0);
        }
    }
    sync_part(s, 0);
    for (int i = 0; i < 3; i++)
        wait_ok(kids[i]);
}

// The exec'd side of `shm` and `memfd`: a program of its own, which maps the
// object itself.
static void peer(int argc, char **argv) {
    CHECK(argc >= 4);
    int fd;
    if (strcmp(argv[2], "shm") == 0) {
        // Made by the other side, which may not have got that far (a
        // process of another client of the host).
        struct stat st;
        for (int tries = 0; (fd = shm_open(argv[3], O_RDWR, 0)) < 0 || fstat(fd, &st) < 0 || st.st_size < (off_t)sizeof(struct sync); tries++) {
            CHECK(tries < 1000);
            if (fd >= 0)
                close(fd);
            sleep_ms(5);
        }
    } else {
        // A memfd's fd, over the Unix socket inherited as argv[3].
        int sock = atoi(argv[3]);
        char byte;
        char cbuf[CMSG_SPACE(sizeof(int))];
        struct iovec iov = { &byte, 1 };
        struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf };
        CHECK(recvmsg(sock, &msg, 0) == 1);
        struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
        CHECK(c && c->cmsg_type == SCM_RIGHTS);
        memcpy(&fd, CMSG_DATA(c), sizeof fd);
    }
    struct sync *s = mmap(NULL, sizeof *s, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(s != MAP_FAILED);
    for (int tries = 0; strcmp(s->data, "hello peer") != 0; tries++) {
        CHECK(tries < 1000);
        sleep_ms(5);
    }
    sync_part(s, 1);
    strcpy(s->data, "hello back");
    _exit(0);
}

static pid_t spawn_peer(char *kind, char *arg) {
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        char *args[] = { self, "peer", kind, arg, NULL };
        execv(self, args);
        _exit(127);
    }
    return pid;
}

// `shm [NAME]`: with a name, the peer is started by someone else (another
// client of the host).
static void test_shm(const char *given) {
    char name[64];
    snprintf(name, sizeof name, "/shmfutex.%d", (int)getpid());
    if (given)
        snprintf(name, sizeof name, "%s", given);
    int fd = shm_open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, sizeof(struct sync)) == 0);
    struct sync *s = mmap(NULL, sizeof *s, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(s != MAP_FAILED);
    sync_init(s, 2);
    strcpy(s->data, "hello peer");
    pid_t pid = given ? 0 : spawn_peer("shm", name);
    sync_part(s, 0);
    if (pid)
        wait_ok(pid);
    for (int tries = 0; strcmp(s->data, "hello back") != 0; tries++) {
        CHECK(tries < 1000);
        sleep_ms(5);
    }
    CHECK(shm_unlink(name) == 0);
}

static void test_memfd(void) {
    int fd = memfd_create("shmfutex", MFD_CLOEXEC);
    CHECK(fd >= 0);
    CHECK(ftruncate(fd, sizeof(struct sync)) == 0);
    struct sync *s = mmap(NULL, sizeof *s, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    CHECK(s != MAP_FAILED);
    sync_init(s, 2);
    strcpy(s->data, "hello peer");
    int sv[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0);
    char arg[16];
    snprintf(arg, sizeof arg, "%d", sv[1]);
    pid_t pid = spawn_peer("memfd", arg);
    close(sv[1]);
    char byte = 'm';
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct iovec iov = { &byte, 1 };
    struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf };
    struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), &fd, sizeof fd);
    CHECK(sendmsg(sv[0], &msg, 0) == 1);
    sync_part(s, 0);
    wait_ok(pid);
    CHECK(strcmp(s->data, "hello back") == 0);
}

// ---- robust mutexes ----

struct robust {
    pthread_mutex_t m;
    atomic_int locked;
};

static void robust_init(struct robust *r, int pi) {
    pthread_mutexattr_t ma;
    pthread_mutexattr_init(&ma);
    CHECK(pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED) == 0);
    CHECK(pthread_mutexattr_setrobust(&ma, PTHREAD_MUTEX_ROBUST) == 0);
    if (pi)
        CHECK(pthread_mutexattr_setprotocol(&ma, PTHREAD_PRIO_INHERIT) == 0);
    CHECK(pthread_mutex_init(&r->m, &ma) == 0);
    r->locked = 0;
}

static void *kill_soon(void *arg) {
    sleep_ms(100);
    kill((pid_t)(intptr_t)arg, SIGKILL);
    return NULL;
}

// The holder (a child) locks and then dies: by _exit, or killed. `waiting`:
// this process is blocked in pthread_mutex_lock as it dies, else it locks
// once the holder has gone.
static void robust_case(struct robust *r, int pi, int killed, int waiting) {
    robust_init(r, pi);
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        CHECK(pthread_mutex_lock(&r->m) == 0);
        r->locked = 1;
        if (killed)
            for (;;) pause();
        sleep_ms(waiting ? 100 : 0);
        _exit(0);
    }
    while (!r->locked)
        sleep_ms(1);
    pthread_t killer;
    if (killed)
        CHECK(pthread_create(&killer, NULL, kill_soon, (void *)(intptr_t)pid) == 0);
    if (!waiting) {
        int st;
        if (killed)
            pthread_join(killer, NULL);
        CHECK(waitpid(pid, &st, 0) == pid);
    }
    int e = pthread_mutex_lock(&r->m);
    if (e != EOWNERDEAD)
        printf("robust pi=%d killed=%d waiting=%d: lock gave %d\n", pi, killed, waiting, e);
    CHECK(e == EOWNERDEAD);
    CHECK(pthread_mutex_consistent(&r->m) == 0);
    CHECK(pthread_mutex_unlock(&r->m) == 0);
    CHECK(pthread_mutex_lock(&r->m) == 0);
    CHECK(pthread_mutex_unlock(&r->m) == 0);
    if (waiting) {
        int st;
        if (killed)
            pthread_join(killer, NULL);
        CHECK(waitpid(pid, &st, 0) == pid);
        CHECK(killed ? WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL : WIFEXITED(st));
    }
}

static void test_robust(int killed) {
    struct robust *r = mmap(NULL, sizeof *r, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(r != MAP_FAILED);
    for (int pi = 0; pi < 2; pi++)
        for (int waiting = 0; waiting < 2; waiting++)
            robust_case(r, pi, killed, waiting);
}

// ---- PI mutexes ----

struct pi {
    pthread_mutex_t m;
    pthread_barrier_t b;
    long counter;
    atomic_int held;
};

static void test_pi(void) {
    struct pi *s = mmap(NULL, sizeof *s, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(s != MAP_FAILED);
    pthread_mutexattr_t ma;
    pthread_mutexattr_init(&ma);
    CHECK(pthread_mutexattr_setpshared(&ma, PTHREAD_PROCESS_SHARED) == 0);
    CHECK(pthread_mutexattr_setprotocol(&ma, PTHREAD_PRIO_INHERIT) == 0);
    CHECK(pthread_mutex_init(&s->m, &ma) == 0);
    pthread_barrierattr_t ba;
    pthread_barrierattr_init(&ba);
    CHECK(pthread_barrierattr_setpshared(&ba, PTHREAD_PROCESS_SHARED) == 0);
    CHECK(pthread_barrier_init(&s->b, &ba, 3) == 0);
    pid_t kids[2];
    for (int i = 0; i < 2; i++) {
        kids[i] = fork();
        CHECK(kids[i] >= 0);
        if (kids[i] == 0) {
            pthread_barrier_wait(&s->b);
            for (int j = 0; j < INCREMENTS; j++) {
                CHECK(pthread_mutex_lock(&s->m) == 0);
                s->counter++;
                CHECK(pthread_mutex_unlock(&s->m) == 0);
            }
            pthread_barrier_wait(&s->b);
            // Hold it while the parent tries.
            if (i == 0) {
                CHECK(pthread_mutex_lock(&s->m) == 0);
                s->held = 1;
                while (s->held == 1)
                    sleep_ms(1);
                CHECK(pthread_mutex_unlock(&s->m) == 0);
            }
            _exit(0);
        }
    }
    pthread_barrier_wait(&s->b);
    for (int j = 0; j < INCREMENTS; j++) {
        CHECK(pthread_mutex_lock(&s->m) == 0);
        s->counter++;
        CHECK(pthread_mutex_unlock(&s->m) == 0);
    }
    pthread_barrier_wait(&s->b);
    CHECK(s->counter == 3L * INCREMENTS);
    while (!s->held)
        sleep_ms(1);
    CHECK(pthread_mutex_trylock(&s->m) == EBUSY);
    s->held = 2;
    // Blocks until the child lets go, then is ours.
    CHECK(pthread_mutex_lock(&s->m) == 0);
    CHECK(pthread_mutex_unlock(&s->m) == 0);
    for (int i = 0; i < 2; i++)
        wait_ok(kids[i]);
}

// ---- raw futex ops across processes ----

struct words {
    int a, b, c;
    atomic_int queued;
    atomic_int woken[8];
};

static struct words *new_words(void) {
    struct words *w = mmap(NULL, sizeof *w, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(w != MAP_FAILED);
    memset(w, 0, sizeof *w);
    return w;
}

// A child waiting on `word` (non-private) while it's 0; then marked woken.
static pid_t waiter(struct words *w, int *word, int i) {
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        w->queued++;
        while (*word == 0) {
            long r = futex(word, FUTEX_WAIT, 0, 0, NULL, 0);
            CHECK(r == 0 || errno == EAGAIN || errno == EINTR);
            if (r == 0)
                break;
        }
        w->woken[i] = 1;
        _exit(0);
    }
    return pid;
}

static int woken_count(struct words *w, int n) {
    int c = 0;
    for (int i = 0; i < n; i++)
        c += w->woken[i];
    return c;
}

static void until_queued(struct words *w, int n) {
    while (w->queued < n)
        sleep_ms(1);
    sleep_ms(100); // into FUTEX_WAIT
}

static void test_requeue(void) {
    struct words *w = new_words();
    pid_t kids[3];
    for (int i = 0; i < 3; i++)
        kids[i] = waiter(w, &w->a, i);
    until_queued(w, 3);
    // A stale expected value: nothing moves.
    errno = 0;
    CHECK(futex(&w->a, FUTEX_CMP_REQUEUE, 1, 2, &w->b, 1) == -1 && errno == EAGAIN);
    // One woken, two moved to b.
    CHECK(futex(&w->a, FUTEX_CMP_REQUEUE, 1, 2, &w->b, 0) == 3);
    sleep_ms(50);
    CHECK(woken_count(w, 3) == 1);
    CHECK(futex(&w->a, FUTEX_WAKE, INT_MAX, 0, NULL, 0) == 0);
    CHECK(futex(&w->b, FUTEX_WAKE, INT_MAX, 0, NULL, 0) == 2);
    for (int i = 0; i < 3; i++)
        wait_ok(kids[i]);
    CHECK(woken_count(w, 3) == 3);
}

static void test_wakeop(void) {
    struct words *w = new_words();
    pid_t on_a = waiter(w, &w->a, 0);
    pid_t on_b = waiter(w, &w->b, 1);
    until_queued(w, 2);
    // b = 1 (it was 0, which passes "== 0"): wake one on a and one on b.
    CHECK(futex(&w->a, FUTEX_WAKE_OP, 1, 1, &w->b, FUTEX_OP(FUTEX_OP_SET, 1, FUTEX_OP_CMP_EQ, 0)) == 2);
    CHECK(w->b == 1);
    wait_ok(on_a);
    wait_ok(on_b);
    // A comparison that fails wakes only on a: b += 1 (1, not > 5).
    w->queued = 0;
    w->a = 0;
    pid_t again = waiter(w, &w->a, 2);
    until_queued(w, 1);
    CHECK(futex(&w->a, FUTEX_WAKE_OP, 1, 1, &w->b, FUTEX_OP(FUTEX_OP_ADD, 1, FUTEX_OP_CMP_GT, 5)) == 1);
    CHECK(w->b == 2);
    wait_ok(again);
}

struct waitv_entry { // struct futex_waitv
    uint64_t val;
    uint64_t uaddr;
    uint32_t flags;
    uint32_t reserved;
};
#define SYS_FUTEX_WAITV 449


static void test_waitv(void) {
    struct words *w = new_words();
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        struct waitv_entry v[3] = {
            { 0, (uintptr_t)&w->a, FUTEX2_SIZE_U32, 0 },
            { 0, (uintptr_t)&w->b, FUTEX2_SIZE_U32, 0 },
            { 0, (uintptr_t)&w->c, FUTEX2_SIZE_U32, 0 },
        };
        // A value that differs: EAGAIN at once.
        w->c = 5;
        errno = 0;
        CHECK(syscall(SYS_FUTEX_WAITV, v, 3, 0, NULL, 0) == -1 && errno == EAGAIN);
        v[2].val = 5;
        w->queued = 1;
        long r = syscall(SYS_FUTEX_WAITV, v, 3, 0, NULL, 0);
        _exit(r == 1 ? 0 : 10 + (int)r);
    }
    until_queued(w, 1);
    w->b = 1;
    CHECK(futex(&w->b, FUTEX_WAKE, 1, 0, NULL, 0) == 1);
    wait_ok(pid);
}

// ---- windows ----

static int pipe_byte(int fd) {
    char b;
    CHECK(read(fd, &b, 1) == 1);
    return b;
}

static void say(int fd, char b) {
    CHECK(write(fd, &b, 1) == 1);
}

// `parent` is a mapping of the object in this process; the child maps it
// afresh with `map` and they write to each other; the child then unmaps
// both its mappings, and this one's view stays.
static void windows_case(const char *what, char *parent, size_t len, char *(*map)(void *), void *arg) {
    int to_child[2], to_parent[2];
    CHECK(pipe(to_child) == 0 && pipe(to_parent) == 0);
    memset(parent, 0, len);
    strcpy(parent, "from the parent");
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        char *mine = map ? map(arg) : parent;
        CHECK(mine != MAP_FAILED);
        CHECK(strcmp(mine, "from the parent") == 0);
        strcpy(mine + len / 2, "from the child");
        say(to_parent[1], 'w');
        CHECK(pipe_byte(to_child[0]) == 'p');
        CHECK(strcmp(mine + 16, "parent again") == 0);
        if (map)
            CHECK(munmap(mine, len) == 0);
        CHECK(munmap(parent, len) == 0);
        say(to_parent[1], 'u');
        _exit(0);
    }
    CHECK(pipe_byte(to_parent[0]) == 'w');
    if (strcmp(parent + len / 2, "from the child") != 0) {
        printf("%s: the parent doesn't see the child's write\n", what);
        _exit(1);
    }
    strcpy(parent + 16, "parent again");
    say(to_child[1], 'p');
    CHECK(pipe_byte(to_parent[0]) == 'u');
    wait_ok(pid);
    // Still mapped here, still shared memory.
    CHECK(strcmp(parent + len / 2, "from the child") == 0);
    strcpy(parent + len / 2, "after");
    CHECK(strcmp(parent + len / 2, "after") == 0);
    close(to_child[0]);
    close(to_child[1]);
    close(to_parent[0]);
    close(to_parent[1]);
}

#define WLEN (64 << 10)

static char *map_fd(void *arg) {
    return mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_SHARED, (int)(intptr_t)arg, 0);
}

static char *map_shm_name(void *arg) {
    int fd = shm_open(arg, O_RDWR, 0);
    CHECK(fd >= 0);
    char *p = mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    return p;
}

static void test_windows(void) {
    // A file.
    char path[64];
    snprintf(path, sizeof path, "/tmp/shmfutex.%d", (int)getpid());
    int file = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    CHECK(file >= 0);
    CHECK(ftruncate(file, WLEN) == 0);
    char *p = mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_SHARED, file, 0);
    CHECK(p != MAP_FAILED);
    windows_case("file", p, WLEN, map_fd, (void *)(intptr_t)file);
    char back[16];
    CHECK(pread(file, back, 6, WLEN / 2) == 6 && memcmp(back, "after", 6) == 0);
    // A private mapping's first write is its own: not the file's, not
    // another private mapping's, not the shared one's.
    char *priv = mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_PRIVATE, file, 0);
    CHECK(priv != MAP_FAILED);
    CHECK(strcmp(priv, "from the parent") == 0);
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        char *mine = mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_PRIVATE, file, 0);
        CHECK(mine != MAP_FAILED);
        strcpy(mine, "child's own");
        CHECK(strcmp(priv, "from the parent") == 0);
        strcpy(priv, "child's copy");
        _exit(0);
    }
    wait_ok(pid);
    CHECK(strcmp(priv, "from the parent") == 0);
    CHECK(strcmp(p, "from the parent") == 0);
    strcpy(priv, "parent's own");
    CHECK(strcmp(p, "from the parent") == 0);
    CHECK(pread(file, back, 16, 0) == 16 && strcmp(back, "from the parent") == 0);
    // Pages it hasn't written still show the file's changes.
    strcpy(p + 8192, "changed");
    CHECK(strcmp(priv + 8192, "changed") == 0);
    CHECK(munmap(priv, WLEN) == 0);
    CHECK(munmap(p, WLEN) == 0);
    close(file);
    unlink(path);

    // POSIX shm.
    char name[64];
    snprintf(name, sizeof name, "/shmfutex.w.%d", (int)getpid());
    int shm = shm_open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
    CHECK(shm >= 0);
    CHECK(ftruncate(shm, WLEN) == 0);
    p = mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_SHARED, shm, 0);
    CHECK(p != MAP_FAILED);
    windows_case("shm", p, WLEN, map_shm_name, name);
    CHECK(munmap(p, WLEN) == 0);
    close(shm);
    shm_unlink(name);

    // A memfd.
    int mfd = memfd_create("windows", 0);
    CHECK(mfd >= 0);
    CHECK(ftruncate(mfd, WLEN) == 0);
    p = mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    CHECK(p != MAP_FAILED);
    windows_case("memfd", p, WLEN, map_fd, (void *)(intptr_t)mfd);
    CHECK(munmap(p, WLEN) == 0);
    close(mfd);

    // Shared anonymous memory, which only a fork passes on.
    p = mmap(NULL, WLEN, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    CHECK(p != MAP_FAILED);
    windows_case("anonymous", p, WLEN, NULL, NULL);
    CHECK(munmap(p, WLEN) == 0);
}

// ---- a library mapped by many ----

static void test_library(const char *file, int n) {
    int fd = open(file, O_RDONLY);
    CHECK(fd >= 0);
    struct stat st;
    CHECK(fstat(fd, &st) == 0);
    int ready[2], hold[2];
    CHECK(pipe(ready) == 0 && pipe(hold) == 0);
    pid_t *kids = calloc(n, sizeof *kids);
    for (int i = 0; i < n; i++) {
        kids[i] = fork();
        CHECK(kids[i] >= 0);
        if (kids[i] == 0) {
            close(hold[1]);
            const volatile char *p = mmap(NULL, st.st_size, PROT_READ, MAP_PRIVATE, fd, 0);
            CHECK(p != MAP_FAILED);
            long sum = 0;
            for (off_t o = 0; o < st.st_size; o += 4096)
                sum += p[o];
            (void)sum;
            say(ready[1], 'r');
            char b;
            read(hold[0], &b, 1); // until the parent goes
            _exit(0);
        }
    }
    close(hold[0]);
    for (int i = 0; i < n; i++)
        CHECK(pipe_byte(ready[0]) == 'r');
    printf("%d mapped\n", n);
    fflush(stdout);
    char b;
    while (read(0, &b, 1) > 0) {}
    close(hold[1]);
    for (int i = 0; i < n; i++)
        wait_ok(kids[i]);
}

int main(int argc, char **argv) {
    self = argv[0];
    setvbuf(stdout, NULL, _IOLBF, 0);
    if (argc < 2)
        return 2;
    const char *test = argv[1];
    if (strcmp(test, "peer") == 0)
        peer(argc, argv);
    else if (strcmp(test, "fork") == 0)
        test_fork();
    else if (strcmp(test, "shm") == 0)
        test_shm(argc > 2 ? argv[2] : NULL);
    else if (strcmp(test, "memfd") == 0)
        test_memfd();
    else if (strcmp(test, "robust") == 0)
        test_robust(argc > 2 && strcmp(argv[2], "kill") == 0);
    else if (strcmp(test, "pi") == 0)
        test_pi();
    else if (strcmp(test, "requeue") == 0)
        test_requeue();
    else if (strcmp(test, "wakeop") == 0)
        test_wakeop();
    else if (strcmp(test, "waitv") == 0)
        test_waitv();
    else if (strcmp(test, "windows") == 0)
        test_windows();
    else if (strcmp(test, "library") == 0 && argc > 3)
        test_library(argv[2], atoi(argv[3]));
    else
        return 2;
    printf("shmfutex %s ok\n", test);
    return 0;
}
