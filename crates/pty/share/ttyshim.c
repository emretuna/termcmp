// ttyshim: makes a session-less PTY slave look foreground-owned to the
// calling shell. termcmp's single-session design keeps the inner PTY slave
// out of any session, so the kernel answers tcgetpgrp(0) with ENOTTY and
// fish 4.x exits at startup ("No TTY for interactive shell"). Interposing
// tcgetpgrp/tcsetpgrp on fd 0 lets fish see itself as foreground owner;
// every other descriptor keeps real kernel semantics.
//
// Interposition uses the __DATA,__interpose section (the only mechanism
// that reliably overrides libSystem syscall wrappers via DYLD_INSERT).
//
// Build (universal):
//   clang -arch arm64 -arch x86_64 -dynamiclib -o ttyshim.dylib ttyshim.c
#define _GNU_SOURCE
#include <dlfcn.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/termios.h>

static int (*orig_tcgetpgrp)(int);
static int (*orig_tcsetpgrp)(int, pid_t);

static int my_tcgetpgrp(int fd) {
    if (fd == STDIN_FILENO) return getpgrp();
    return orig_tcgetpgrp(fd);
}

static int my_tcsetpgrp(int fd, pid_t pgrp) {
    // The session-less slave cannot take a fg pgrp; report success so the
    // shell proceeds. Real job control is driven by termcmp's mirror.
    if (fd == STDIN_FILENO) return 0;
    return orig_tcsetpgrp(fd, pgrp);
}

__attribute__((constructor)) static void init(void) {
    orig_tcgetpgrp = dlsym(RTLD_NEXT, "tcgetpgrp");
    orig_tcsetpgrp = dlsym(RTLD_NEXT, "tcsetpgrp");

    // By constructor time dyld has already consumed DYLD_INSERT_LIBRARIES,
    // so rewriting it only changes what child processes inherit: strip this
    // shim's own entry so descendants (direnv, herdr, ...) never load the
    // interpose, while foreign entries pass through untouched. Fail open:
    // if the own path cannot be resolved, leave the variable alone.
    Dl_info info;
    if (dladdr((void*)my_tcgetpgrp, &info) == 0 || info.dli_fname == NULL)
        return;
    const char* inserted = getenv("DYLD_INSERT_LIBRARIES");
    if (inserted == NULL || *inserted == '\0')
        return;

    // dladdr reports the symlink-resolved load path (/private/var/... for a
    // /var/... injection), so byte-compare against dli_fname alone would miss
    // the entry. Match each entry against both the raw dladdr path and the
    // realpath()-normalized form.
    char own_real[PATH_MAX];
    const char* own_normalized =
        realpath(info.dli_fname, own_real) != NULL ? own_real : info.dli_fname;
    char* rebuilt = malloc(strlen(inserted) + 1);
    if (rebuilt == NULL)
        return;
    size_t out = 0;
    const char* p = inserted;
    for (;;) {
        const char* sep = strchr(p, ':');
        size_t len = sep ? (size_t)(sep - p) : strlen(p);
        int match = 0;
        // Early-initialization context: avoid heap allocation entirely.
        if (len < PATH_MAX) {
            char entry[PATH_MAX];
            char entry_real[PATH_MAX];
            memcpy(entry, p, len);
            entry[len] = '\0';
            match = strcmp(entry, info.dli_fname) == 0 ||
                    (realpath(entry, entry_real) != NULL &&
                     strcmp(entry_real, own_normalized) == 0);
        }
        if (!match) {
            if (out > 0) rebuilt[out++] = ':';
            memcpy(rebuilt + out, p, len);
            out += len;
        }
        if (sep == NULL) break;
        p = sep + 1;
    }
    rebuilt[out] = '\0';
    if (out > 0)
        setenv("DYLD_INSERT_LIBRARIES", rebuilt, 1);
    else
        unsetenv("DYLD_INSERT_LIBRARIES");
    free(rebuilt);
}

__attribute__((used)) static struct {
    void* replacement;
    void* replacee;
} interposers[] __attribute__((section("__DATA,__interpose"))) = {
    { (void*)my_tcgetpgrp, (void*)tcgetpgrp },
    { (void*)my_tcsetpgrp, (void*)tcsetpgrp },
};
