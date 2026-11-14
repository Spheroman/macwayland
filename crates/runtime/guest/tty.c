// Terminals and job control, on a pty this program opens (as foot does):
//
//   tty rules                 the line discipline's and job control's rules,
//                             process by process
//   tty bash BASH COREUTILS   an interactive bash on the pty, driven through
//                             the master as a person would type: a job
//                             stopped by SIGTTIN and brought back with `fg`,
//                             Ctrl-Z, `jobs`, `bg`, Ctrl-C reaching only the
//                             foreground job, and a hangup
//
// Prints "tty ok" if everything held.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); fflush(stdout); _exit(1); } } while (0)

static void msleep(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {}
}

// A new pty: its master, and its slave's name.
static int new_pty(char *name, size_t len) {
    int m = posix_openpt(O_RDWR | O_NOCTTY);
    CHECK(m >= 0);
    CHECK(grantpt(m) == 0 && unlockpt(m) == 0);
    CHECK(ptsname_r(m, name, len) == 0);
    return m;
}

// In a new session, take the pty `name` as controlling terminal; returns
// the slave.
static int become_session(const char *name) {
    CHECK(setsid() == getpid());
    int s = open(name, O_RDWR);
    CHECK(s >= 0);
    // Linux makes it the controlling terminal on open; TIOCSCTTY says so
    // either way (and again is fine).
    CHECK(ioctl(s, TIOCSCTTY, 0) == 0);
    CHECK(ioctl(s, TIOCSCTTY, 0) == 0);
    return s;
}

// TTY_TRACE=1: say what's waited for, to find a step that hangs.
static int wait_status(pid_t pid, int options) {
    if (getenv("TTY_TRACE")) fprintf(stderr, "tty %d: wait %d %d\n", getpid(), pid, options);
    int st;
    pid_t got;
    while ((got = waitpid(pid, &st, options)) < 0 && errno == EINTR) {}
    CHECK(got == pid);
    return st;
}

// Make `pgrp` the terminal's foreground group from wherever the caller is
// (bash's way: SIGTTOU blocked around it).
static void foreground(int tty, pid_t pgrp) {
    sigset_t s, old;
    sigemptyset(&s);
    sigaddset(&s, SIGTTOU);
    sigprocmask(SIG_BLOCK, &s, &old);
    CHECK(tcsetpgrp(tty, pgrp) == 0);
    sigprocmask(SIG_SETMASK, &old, 0);
    CHECK(tcgetpgrp(tty) == pgrp);
}

// A child in a process group of its own, running `fn`. (It moves itself,
// and says when it has: a parent's setpgid on a child that has already
// stopped can't be answered in 1:1 mode, where the child does it.)
static pid_t job(void (*fn)(int), int tty) {
    int ready[2];
    CHECK(pipe(ready) == 0);
    pid_t pid = fork();
    CHECK(pid >= 0);
    if (pid == 0) {
        close(ready[0]);
        CHECK(setpgid(0, 0) == 0);
        close(ready[1]);
        fn(tty);
        _exit(0);
    }
    close(ready[1]);
    char c;
    CHECK(read(ready[0], &c, 1) == 0);
    close(ready[0]);
    CHECK(getpgid(pid) == pid);
    return pid;
}

static volatile sig_atomic_t got_sig;
static void on_sig(int sig) { got_sig = sig; }

static void read_one(int tty) {
    char c;
    _exit(read(tty, &c, 1) == 1 && c == 'x' ? 0 : 3);
}

static void read_ignoring_ttin(int tty) {
    signal(SIGTTIN, SIG_IGN);
    char c;
    _exit(read(tty, &c, 1) < 0 && errno == EIO ? 0 : 3);
}

static void write_out(int tty) {
    _exit(write(tty, "w", 1) == 1 ? 0 : 3);
}

static void change_settings(int tty) {
    struct termios t;
    CHECK(tcgetattr(tty, &t) == 0);
    _exit(tcsetattr(tty, TCSANOW, &t) == 0 ? 0 : 3);
}

static void change_settings_blocked(int tty) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGTTOU);
    sigprocmask(SIG_BLOCK, &s, 0);
    change_settings(tty);
}

static void pause_forever(int tty) {
    (void)tty;
    for (;;) pause();
}

static int report[2];
static void count_int(int sig) {
    (void)sig;
    write(report[1], "I", 1);
}
static void count_hup(int sig) {
    (void)sig;
    write(report[1], "H", 1);
}

static void catch_int_forever(int tty) {
    (void)tty;
    signal(SIGINT, count_int);
    write(report[1], "R", 1);
    for (;;) pause();
}

// Wait for `sig`, having said "R" once its handler is in (blocked
// meanwhile, so it can't come between the look and the wait).
static void await_signal(int sig) {
    sigset_t s, old;
    sigemptyset(&s);
    sigaddset(&s, sig);
    sigprocmask(SIG_BLOCK, &s, &old);
    signal(sig, on_sig);
    write(report[1], "R", 1);
    while (got_sig != sig) sigsuspend(&old);
    sigprocmask(SIG_SETMASK, &old, 0);
}

static void winch_waiter(int tty) {
    sigset_t s;
    sigemptyset(&s);
    sigaddset(&s, SIGWINCH);
    sigprocmask(SIG_BLOCK, &s, 0);
    write(report[1], "R", 1);
    for (;;) {
        struct timespec ts = {10, 0};
        if (sigtimedwait(&s, 0, &ts) == SIGWINCH) _exit(0);
        if (getenv("TTY_TRACE")) {
            struct winsize ws = {0};
            ioctl(tty, TIOCGWINSZ, &ws);
            fprintf(stderr, "tty %d: no SIGWINCH yet: size %dx%d, foreground %d, mine %d\n", getpid(), ws.ws_row, ws.ws_col, tcgetpgrp(tty), getpgrp());
        }
    }
}

static void read_line(int tty) {
    // Not before it's the foreground: from the background the read would
    // stop it (SIGTTIN).
    while (tcgetpgrp(tty) != getpgrp()) msleep(5);
    char buf[16];
    ssize_t n = read(tty, buf, sizeof buf);
    _exit(n == 2 && buf[0] == 3 && buf[1] == '\n' ? 0 : 3);
}

// The session of the rules: run as the leader, on pty `name`, with the
// master `m` (anyone can write it: what's written is typed).
static void session_rules(const char *name, int m) {
    int tty = become_session(name);
    CHECK(tcgetsid(tty) == getpid());
    CHECK(tcgetpgrp(tty) == getpgrp());
    int dev = open("/dev/tty", O_RDWR);
    CHECK(dev >= 0);
    close(dev);
    struct termios t;
    CHECK(tcgetattr(tty, &t) == 0);
    t.c_lflag |= ISIG | ICANON | IEXTEN;
    t.c_lflag &= ~(ECHO | TOSTOP);
    t.c_cc[VINTR] = 3;
    t.c_cc[VSUSP] = 0x1a;
    t.c_cc[VQUIT] = 0x1c;
    t.c_cc[VLNEXT] = 0x16;
    CHECK(tcsetattr(tty, TCSANOW, &t) == 0);
    CHECK(pipe(report) == 0);
    char c;

    // A background read stops its job with SIGTTIN; in the foreground and
    // continued, the same read goes on and reads.
    pid_t j = job(read_one, tty);
    int st = wait_status(j, WUNTRACED);
    CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGTTIN);
    foreground(tty, j);
    CHECK(kill(-j, SIGCONT) == 0);
    CHECK(write(m, "x\n", 2) == 2);
    st = wait_status(j, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    foreground(tty, getpgrp());
    char flush[8];
    CHECK(tcflush(tty, TCIFLUSH) == 0);
    (void)flush;

    // Ignoring SIGTTIN, a background read fails with EIO.
    j = job(read_ignoring_ttin, tty);
    st = wait_status(j, WUNTRACED);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);

    // Background writes go out without TOSTOP, stop with it.
    j = job(write_out, tty);
    st = wait_status(j, WUNTRACED);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    t.c_lflag |= TOSTOP;
    CHECK(tcsetattr(tty, TCSANOW, &t) == 0);
    j = job(write_out, tty);
    st = wait_status(j, WUNTRACED);
    CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGTTOU);
    CHECK(kill(-j, SIGKILL) == 0);
    wait_status(j, 0);
    t.c_lflag &= ~TOSTOP;
    CHECK(tcsetattr(tty, TCSANOW, &t) == 0);

    // Changing the terminal from the background: SIGTTOU, or (blocked)
    // it goes ahead.
    j = job(change_settings, tty);
    st = wait_status(j, WUNTRACED);
    CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGTTOU);
    CHECK(kill(-j, SIGKILL) == 0);
    wait_status(j, 0);
    j = job(change_settings_blocked, tty);
    st = wait_status(j, WUNTRACED);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);

    // tcsetpgrp to a group of another session, or none: refused.
    errno = 0;
    CHECK(tcsetpgrp(tty, 1) < 0 && (errno == EPERM || errno == ESRCH));

    // Ctrl-C and Ctrl-\ go to the foreground job, not a background one.
    pid_t bg = job(catch_int_forever, tty);
    CHECK(read(report[0], &c, 1) == 1 && c == 'R');
    j = job(pause_forever, tty);
    foreground(tty, j);
    msleep(50);
    CHECK(write(m, "\x03", 1) == 1);
    st = wait_status(j, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGINT);
    j = job(pause_forever, tty);
    foreground(tty, j);
    msleep(50);
    CHECK(write(m, "\x1c", 1) == 1);
    st = wait_status(j, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGQUIT);
    // Ctrl-Z stops it; continued, it carries on.
    j = job(pause_forever, tty);
    foreground(tty, j);
    msleep(50);
    CHECK(write(m, "\x1a", 1) == 1);
    st = wait_status(j, WUNTRACED);
    CHECK(WIFSTOPPED(st) && WSTOPSIG(st) == SIGTSTP);
    CHECK(kill(-j, SIGCONT) == 0);
    st = wait_status(j, WCONTINUED);
    CHECK(WIFCONTINUED(st));
    CHECK(kill(-j, SIGKILL) == 0);
    wait_status(j, 0);
    // The background job heard nothing.
    struct pollfd pfd = {report[0], POLLIN, 0};
    CHECK(poll(&pfd, 1, 100) == 0);
    CHECK(kill(-bg, SIGKILL) == 0);
    wait_status(bg, 0);

    // Ctrl-V quotes the next character: a ^C to read, not a signal.
    j = job(read_line, tty);
    foreground(tty, j);
    msleep(50);
    CHECK(write(m, "\x16\x03\n", 3) == 3);
    st = wait_status(j, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    foreground(tty, getpgrp());

    // A new size, set through the master: SIGWINCH to the foreground.
    j = job(winch_waiter, tty);
    CHECK(read(report[0], &c, 1) == 1 && c == 'R');
    foreground(tty, j);
    struct winsize ws = {40, 100, 0, 0}, was;
    CHECK(ioctl(m, TIOCGWINSZ, &was) == 0);
    CHECK(ioctl(m, TIOCSWINSZ, &ws) == 0);
    if (getenv("TTY_TRACE")) fprintf(stderr, "tty %d: size %dx%d to 40x100, foreground %d\n", getpid(), was.ws_row, was.ws_col, tcgetpgrp(tty));
    st = wait_status(j, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    foreground(tty, getpgrp());

    // A group whose last link in the session goes is orphaned: a read
    // from the background fails with EIO rather than stopping it for good.
    pid_t mid = fork();
    CHECK(mid >= 0);
    if (mid == 0) {
        setpgid(0, 0);
        pid_t self = getpid();
        pid_t orphan = fork();
        if (orphan == 0) {
            // Until its parent (the group's link to the session leader's
            // group) has gone.
            while (getppid() == self) msleep(5);
            char ch;
            ssize_t n = read(tty, &ch, 1);
            write(report[1], n < 0 && errno == EIO ? "E" : "?", 1);
            _exit(0);
        }
        _exit(0);
    }
    wait_status(mid, 0);
    CHECK(read(report[0], &c, 1) == 1);
    CHECK(c == 'E');

    // A group orphaned with a stopped process in it would stay stopped for
    // good: it gets SIGHUP, then SIGCONT.
    mid = fork();
    CHECK(mid >= 0);
    if (mid == 0) {
        setpgid(0, 0);
        pid_t stopped = fork();
        if (stopped == 0) {
            signal(SIGHUP, count_hup);
            kill(getpid(), SIGSTOP);
            // Continued, after the hangup.
            write(report[1], "C", 1);
            _exit(0);
        }
        int s2 = wait_status(stopped, WUNTRACED);
        _exit(WIFSTOPPED(s2) ? 0 : 1);
    }
    st = wait_status(mid, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    char hc[2];
    CHECK(read(report[0], &hc[0], 1) == 1 && read(report[0], &hc[1], 1) == 1);
    CHECK(hc[0] == 'H' && hc[1] == 'C');

    // A process that isn't the leader gives the terminal up: it has none.
    pid_t k = fork();
    if (k == 0) {
        CHECK(ioctl(tty, TIOCNOTTY) == 0);
        CHECK(tcgetpgrp(tty) < 0 && errno == ENOTTY);
        CHECK(open("/dev/tty", O_RDWR) < 0 && errno == ENXIO);
        _exit(0);
    }
    st = wait_status(k, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    // Not a session leader: no taking a terminal.
    k = fork();
    if (k == 0) {
        setpgid(0, 0);
        CHECK(ioctl(tty, TIOCSCTTY, 0) < 0 && errno == EPERM);
        _exit(0);
    }
    st = wait_status(k, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    _exit(0);
}

// A session whose leader waits for SIGHUP, to be hung up by its master's
// close: SIGHUP and SIGCONT to the leader.
static void session_hangup(const char *name, int m) {
    close(m);
    int tty = become_session(name);
    (void)tty;
    await_signal(SIGHUP);
    write(report[1], "H", 1);
    _exit(0);
}

// A session whose leader ends with a stopped job in the foreground... and
// a job left: the foreground group gets SIGHUP as the leader goes.
static void session_leader_exit(const char *name, int m) {
    close(m);
    int tty = become_session(name);
    pid_t j = job(pause_forever, tty);
    foreground(tty, j);
    char pid[16];
    int n = snprintf(pid, sizeof pid, "%d\n", j);
    write(report[1], pid, n);
    _exit(0);
}

static void rules(void) {
    char name[64];
    int m = new_pty(name, sizeof name);
    pid_t s = fork();
    if (s == 0) session_rules(name, m);
    int st = wait_status(s, 0);
    CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0);
    close(m);

    CHECK(pipe(report) == 0);
    m = new_pty(name, sizeof name);
    s = fork();
    if (s == 0) session_hangup(name, m);
    char c;
    CHECK(read(report[0], &c, 1) == 1 && c == 'R');
    close(m);
    CHECK(read(report[0], &c, 1) == 1 && c == 'H');
    st = wait_status(s, 0);
    CHECK(WIFEXITED(st));

    m = new_pty(name, sizeof name);
    s = fork();
    if (s == 0) session_leader_exit(name, m);
    char buf[16] = {0};
    for (size_t i = 0; i < sizeof buf - 1 && read(report[0], buf + i, 1) == 1 && buf[i] != '\n'; i++) {}
    pid_t j = atoi(buf);
    CHECK(j > 0);
    wait_status(s, 0);
    // The job (now init's) ends with the hangup.
    int gone = 0;
    for (int i = 0; i < 200 && !gone; i++) {
        gone = kill(j, 0) < 0 && errno == ESRCH;
        if (!gone) msleep(10);
    }
    CHECK(gone);
    close(m);
}

// ---- bash, driven through the master ----

static int bm;            // the master
static char seen[1 << 16];
static size_t nseen;

// Read the master until `want` has appeared since `from` (an offset into
// what's been seen), or fail after `ms`.
static size_t expect_from(size_t from, const char *want, int ms) {
    for (int waited = 0;;) {
        seen[nseen] = 0;
        char *at = strstr(seen + from, want);
        if (at) return at - seen + strlen(want);
        struct pollfd p = {bm, POLLIN, 0};
        if (poll(&p, 1, 50) > 0) {
            ssize_t n = read(bm, seen + nseen, sizeof seen - 1 - nseen);
            if (n > 0) nseen += n;
        } else if ((waited += 50) > ms) {
            printf("FAIL: no \"%s\" after:\n%s\n", want, seen + (nseen > 2000 ? nseen - 2000 : 0));
            fflush(stdout);
            _exit(1);
        }
    }
}

static size_t mark;
static void expect(const char *want) { mark = expect_from(mark, want, 10000); }
static void type(const char *s) { CHECK(write(bm, s, strlen(s)) == (ssize_t)strlen(s)); }

static void bash(const char *bash_path, const char *coreutils) {
    char name[64];
    bm = new_pty(name, sizeof name);
    struct winsize ws = {24, 80, 0, 0};
    ioctl(bm, TIOCSWINSZ, &ws);
    pid_t sh = fork();
    if (sh == 0) {
        close(bm);
        int tty = become_session(name);
        dup2(tty, 0);
        dup2(tty, 1);
        dup2(tty, 2);
        if (tty > 2) close(tty);
        char path[512];
        snprintf(path, sizeof path, "PATH=%s/bin", coreutils);
        char *env[] = {path, "PS1=$ ", "TERM=dumb", "HOME=/tmp", 0};
        execle(bash_path, "bash", "--norc", "--noprofile", "--noediting", "-i", (char *)0, env);
        _exit(127);
    }
    expect("$ ");

    // A background job reading the terminal stops (SIGTTIN); `fg` brings
    // it back, and it reads what's typed.
    type("cat &\n");
    expect("$ ");
    type("sleep 0.3; jobs; echo J1-$((1+1))\n");
    expect("Stopped");
    expect("J1-2");
    type("fg\n");
    expect("cat");
    msleep(200);
    type("hello\n");
    expect("hello");
    expect("hello");
    type("\x04");
    type("echo F-$((2+2))\n");
    expect("F-4");

    // Ctrl-Z stops the foreground job; `jobs` says so; `bg` runs it on.
    type("echo z-$((8+8)); sleep 100\n");
    expect("z-16");
    msleep(300);
    type("\x1a");
    expect("Stopped");
    type("jobs; echo J2-$((1+2))\n");
    expect("Stopped");
    expect("J2-3");
    type("bg\n");
    expect("sleep 100 &");
    type("jobs; echo J3-$((1+3))\n");
    expect("Running");
    expect("J3-4");
    type("kill %1; wait; echo K-$((1+4))\n");
    expect("K-5");

    // Ctrl-C ends the foreground job, and only it.
    type("sleep 101 & echo bg-$((9+9))\n");
    expect("bg-18");
    type("echo go-$((7+7)); sleep 100\n");
    expect("go-14");
    msleep(300);
    type("\x03");
    type("echo S=$?; jobs; echo C-$((3+3))\n");
    expect("S=130");
    expect("Running");
    expect("C-6");
    type("kill %1; wait; echo W-$((2+9))\n");
    expect("W-11");

    // Hangup: the master closes, bash (the session leader) gets SIGHUP,
    // hangs up its jobs, and ends by it.
    type("sleep 102 & printf 'P%s=%s:\\n' X $!\n");
    expect("PX=");
    size_t at = mark;
    expect(":");
    pid_t job_pid = atoi(seen + at);
    CHECK(job_pid > 0 && kill(job_pid, 0) == 0);
    expect("$ ");
    close(bm);
    int st = wait_status(sh, 0);
    CHECK(WIFSIGNALED(st) && WTERMSIG(st) == SIGHUP);
    int gone = 0;
    for (int i = 0; i < 300 && !gone; i++) {
        gone = kill(job_pid, 0) < 0 && errno == ESRCH;
        if (!gone) msleep(10);
    }
    CHECK(gone);
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    if (argc >= 2 && strcmp(argv[1], "rules") == 0) {
        rules();
    } else if (argc >= 4 && strcmp(argv[1], "bash") == 0) {
        bash(argv[2], argv[3]);
    } else {
        fprintf(stderr, "usage: tty rules | tty bash BASH COREUTILS\n");
        return 2;
    }
    printf("tty ok\n");
    return 0;
}
