// Page out a small sentinel, then verify it after capture and restore.
#define _GNU_SOURCE
#include <signal.h>
#include <stdio.h>
#include <sys/mman.h>
#include <unistd.h>

static volatile sig_atomic_t verify;
static void wake(int signal) { (void)signal; verify = 1; }

int main(void) {
    const size_t size = 4 * 1024 * 1024;
    unsigned char *p = mmap(NULL, size, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (p == MAP_FAILED) return 1;
    signal(SIGUSR1, wake);
    for (size_t i = 0; i < size; i++) p[i] = (unsigned char)(i % 251);
    if (madvise(p, size, MADV_PAGEOUT) != 0) { perror("pageout"); return 2; }
    FILE *ready = fopen("/tmp/pageout-ready", "w");
    if (!ready) return 3;
    fclose(ready);
    // Block the signal around the condition to avoid a lost wakeup.
    sigset_t blocked, previous;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR1);
    sigprocmask(SIG_BLOCK, &blocked, &previous);
    while (!verify) sigsuspend(&previous);
    for (size_t i = 0; i < size; i++) {
        if (p[i] != (unsigned char)(i % 251)) return 4;
    }
    FILE *ok = fopen("/tmp/pageout-ok", "w");
    if (!ok) return 5;
    fclose(ok);
    return 0;
}
