#include <stdio.h>

int main(int argc, char **argv) {
    printf("hello from musl, argc=%d argv[0]=%s\n", argc, argv[0]);
    return 7;
}
