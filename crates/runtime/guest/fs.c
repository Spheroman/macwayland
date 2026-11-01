// Filesystem checks through ordinary libc calls. argv[1] is a scratch
// directory (an absolute path without symlinks).
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#define CHECK(cond) do { if (!(cond)) { printf("FAIL line %d: %s (errno %d)\n", __LINE__, #cond, errno); return 1; } } while (0)

int main(int argc, char **argv) {
    char buf[4096];
    CHECK(argc == 2);
    CHECK(chdir(argv[1]) == 0);
    CHECK(getcwd(buf, sizeof buf) && strcmp(buf, argv[1]) == 0);
    CHECK(mkdir("sub", 0755) == 0);

    FILE *f = fopen("sub/a.txt", "w");
    CHECK(f);
    fputs("alpha\n", f);
    CHECK(fclose(f) == 0);

    CHECK(symlink("a.txt", "sub/link") == 0);
    ssize_t n = readlink("sub/link", buf, sizeof buf);
    CHECK(n == 5 && memcmp(buf, "a.txt", 5) == 0);
    struct stat st;
    CHECK(stat("sub/link", &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 6);
    CHECK(lstat("sub/link", &st) == 0 && S_ISLNK(st.st_mode));
    f = fopen("sub/link", "r");
    CHECK(f && fgets(buf, sizeof buf, f) && strcmp(buf, "alpha\n") == 0);
    fclose(f);

    DIR *d = opendir("sub");
    CHECK(d);
    int count = 0;
    for (struct dirent *e; (e = readdir(d));)
        count++;
    closedir(d);
    CHECK(count == 4); // ., .., a.txt, link

    CHECK(rename("sub/a.txt", "sub/b.txt") == 0);
    CHECK(access("sub/a.txt", F_OK) == -1 && errno == ENOENT);
    CHECK(access("sub/b.txt", R_OK | W_OK) == 0);

    n = readlink("/proc/self/exe", buf, sizeof buf);
    CHECK(n > 0 && buf[0] == '/');
    int fd = open("/dev/null", O_WRONLY | O_CLOEXEC);
    CHECK(fd >= 0 && write(fd, "x", 1) == 1);
    CHECK(fcntl(fd, F_GETFD) == FD_CLOEXEC);
    CHECK(close(fd) == 0);

    CHECK(unlink("sub/link") == 0 && unlink("sub/b.txt") == 0 && rmdir("sub") == 0);
    puts("fs ok");
    return 0;
}
