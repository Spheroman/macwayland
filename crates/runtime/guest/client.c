// A Linux process for a client to stand in for (docs/processes.md, step
// 5). Run as `client TEST [ARGS]`, TEST one of:
//
//   exit N        exit with status N
//   raise SIG     end by signal SIG (its default action)
//   trap          print "ready PID PGID SID PPID", wait for a signal, and
//                 print "got SIG" and exit with 100 + SIG on the first one
//                 (SIGHUP, SIGINT, SIGTERM, SIGQUIT, SIGUSR1)
//   wait          print "ready PID", and wait for stdin to close
//   daemon SECS   leave a child behind in a session of its own, living
//                 SECS seconds, and exit at once
//   stdio         say which of fds 0-2 are open and what they are
//
// Every line it prints is flushed at once.
#define _GNU_SOURCE
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void got(int sig) {
    char line[32];
    int n = snprintf(line, sizeof line, "got %d\n", sig);
    write(1, line, n);
    _exit(100 + sig);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    const char *test = argc > 1 ? argv[1] : "";
    if (!strcmp(test, "exit") && argc > 2) {
        return atoi(argv[2]);
    }
    if (!strcmp(test, "raise") && argc > 2) {
        int sig = atoi(argv[2]);
        signal(sig, SIG_DFL);
        raise(sig);
        return 1;
    }
    if (!strcmp(test, "trap")) {
        int sigs[] = {SIGHUP, SIGINT, SIGTERM, SIGQUIT, SIGUSR1};
        for (unsigned i = 0; i < sizeof sigs / sizeof *sigs; i++) {
            signal(sigs[i], got);
        }
        printf("ready %d %d %d %d\n", getpid(), getpgrp(), getsid(0), getppid());
        for (;;) {
            pause();
        }
    }
    if (!strcmp(test, "wait")) {
        printf("ready %d\n", getpid());
        char c;
        while (read(0, &c, 1) > 0) {
        }
        return 0;
    }
    if (!strcmp(test, "daemon") && argc > 2) {
        int secs = atoi(argv[2]);
        pid_t pid = fork();
        if (pid < 0) {
            return 1;
        }
        if (pid == 0) {
            setsid();
            // Nothing of the client's held open.
            for (int fd = 0; fd < 3; fd++) {
                close(fd);
            }
            sleep(secs);
            _exit(0);
        }
        printf("daemon %d\n", pid);
        return 0;
    }
    if (!strcmp(test, "stdio")) {
        for (int fd = 0; fd < 3; fd++) {
            struct stat st;
            if (fstat(fd, &st) != 0) {
                printf("%d closed\n", fd);
            } else {
                printf("%d %s\n", fd, S_ISFIFO(st.st_mode) ? "fifo" : S_ISCHR(st.st_mode) ? (isatty(fd) ? "tty" : "chr") : S_ISREG(st.st_mode) ? "file" : S_ISSOCK(st.st_mode) ? "socket" : "other");
            }
        }
        return 0;
    }
    fprintf(stderr, "usage: client exit N | raise SIG | trap | wait | daemon SECS | stdio\n");
    return 2;
}
