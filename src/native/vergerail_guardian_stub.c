/*
 * The guardian is deliberately unavailable outside aarch64 macOS.  Keeping
 * this tiny target-neutral executable lets the Rust crate cross-compile and
 * retain its typed Unsupported runtime path without importing Darwin headers.
 */
#include <stddef.h>

#define EXIT_UNSUPPORTED 78

int main(int argc, char **argv) {
    (void)argc;
    (void)argv;
    return EXIT_UNSUPPORTED;
}
