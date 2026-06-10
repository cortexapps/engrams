// ADR 0045 Phase C2 gate-opener: the pagemap-anon dirty map.
//
// The post-copy source classifies "chunks the destination must pull from
// me" WITHOUT any capture and WITHOUT a KVM-dirty-bitmap fork surface, by
// walking /proc/<pid>/pagemap over the guest VMAs: under the substrate
// (MAP_PRIVATE of a shm base file, UFFD MISSING|MINOR), divergent pages
// are ANONYMOUS (UFFDIO_COPY-installed or guest-COW) while clean pages
// are file-backed, and untouched pages are absent. ALT_SOURCE absorbs the
// over-approximation (anon-but-still-durable).
//
// This probe proves the three load-bearing kernel behaviors from a
// PARENT process (the host-agent's seat — it is FC's parent, so
// process_vm_readv is YAMA-legal):
//
//   P0  hole in the base file  -> child read  -> MISSING -> UFFDIO_COPY
//       expect: present + ANON (bit 61 clear); readv == COPY pattern
//   P1  present in base        -> child read (MINOR->CONTINUE) then WRITE
//       expect: present + ANON (COW); readv == COW pattern
//   P2  present in base        -> child read only (MINOR->CONTINUE)
//       expect: present + FILE (bit 61 set); readv == base pattern
//   P3  untouched              -> expect: NOT present; then readv FAULTS
//       it through the child's own uffd handler (MINOR->CONTINUE) and
//       returns base bytes — the "readv on a MISSING/unfaulted page
//       triggers the source's own handler fill" correctness arm.
//
// T1 pagemap classification P0-P3; T2 readv bytes P0-P2; T3 readv-through-
// fault on P3 + post-readv reclassification to FILE. Any FAIL => the C2
// pagemap design falls back to a KVM-dirty-bitmap fork surface.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <linux/userfaultfd.h>
#include <poll.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#define NPAGES 4
#define BASE_BYTE 0xB5
#define COPY_BYTE 0xC0
#define COW_BYTE 0xCB

static long PS;

static int fail(const char *what) {
  printf("FAIL %s (errno=%d %m)\n", what, errno);
  fflush(stdout);
  return 1;
}

struct handler_args {
  int uffd;
  uint8_t *map;   // child mapping base
  int shm_fd;     // base file (for CONTINUE the kernel uses the mapping)
};

// Child-side fault handler: MINOR -> CONTINUE, MISSING -> COPY(COPY_BYTE).
static void *handler(void *argp) {
  struct handler_args *a = argp;
  for (;;) {
    struct uffd_msg msg;
    ssize_t n = read(a->uffd, &msg, sizeof(msg));
    if (n <= 0)
      return NULL;
    if (msg.event != UFFD_EVENT_PAGEFAULT)
      continue;
    uint64_t addr = msg.arg.pagefault.address & ~(PS - 1);
    if (msg.arg.pagefault.flags & UFFD_PAGEFAULT_FLAG_MINOR) {
      struct uffdio_continue cont = {
          .range = {.start = addr, .len = (unsigned long)PS}, .mode = 0};
      ioctl(a->uffd, UFFDIO_CONTINUE, &cont);
    } else {
      static uint8_t page[1 << 16];
      memset(page, COPY_BYTE, PS);
      struct uffdio_copy cp = {.dst = addr,
                               .src = (unsigned long)page,
                               .len = (unsigned long)PS,
                               .mode = 0};
      ioctl(a->uffd, UFFDIO_COPY, &cp);
    }
  }
}

static int child_main(int shm_fd, int ready_w) {
  uint8_t *map = mmap(NULL, NPAGES * PS, PROT_READ | PROT_WRITE, MAP_PRIVATE,
                      shm_fd, 0);
  if (map == MAP_FAILED)
    return fail("child mmap");

  int uffd = syscall(SYS_userfaultfd, O_CLOEXEC);
  if (uffd < 0)
    return fail("userfaultfd");
  struct uffdio_api api = {.api = UFFD_API, .features = UFFD_FEATURE_MINOR_SHMEM};
  if (ioctl(uffd, UFFDIO_API, &api))
    return fail("UFFDIO_API");
  struct uffdio_register reg = {
      .range = {.start = (unsigned long)map, .len = NPAGES * PS},
      .mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_MINOR};
  if (ioctl(uffd, UFFDIO_REGISTER, &reg))
    return fail("UFFDIO_REGISTER MISSING|MINOR");

  struct handler_args a = {.uffd = uffd, .map = map, .shm_fd = shm_fd};
  pthread_t th;
  pthread_create(&th, NULL, handler, &a);

  volatile uint8_t sink;
  sink = map[0 * PS];        // P0: hole -> MISSING -> COPY
  sink = map[1 * PS];        // P1: MINOR -> CONTINUE...
  map[1 * PS] = COW_BYTE;    //     ...then COW write
  sink = map[2 * PS];        // P2: MINOR -> CONTINUE, read-only
  (void)sink;                // P3: untouched

  // Hand the parent our mapping base, then park.
  uint64_t base = (uint64_t)(uintptr_t)map;
  if (write(ready_w, &base, sizeof(base)) != sizeof(base))
    return fail("child ready write");
  pause();
  return 0;
}

struct pm {
  int present;
  int file_backed;
};

static int read_pagemap(int pm_fd, uint64_t vaddr, struct pm *out) {
  uint64_t entry;
  if (pread(pm_fd, &entry, 8, (off_t)(vaddr / PS * 8)) != 8)
    return -1;
  out->present = (entry >> 63) & 1;
  out->file_backed = (entry >> 61) & 1;
  return 0;
}

static int readv_page(pid_t pid, uint64_t vaddr, uint8_t *buf) {
  struct iovec local = {.iov_base = buf, .iov_len = PS};
  struct iovec remote = {.iov_base = (void *)(uintptr_t)vaddr, .iov_len = PS};
  return process_vm_readv(pid, &local, 1, &remote, 1, 0) == PS ? 0 : -1;
}

int main(void) {
  PS = sysconf(_SC_PAGESIZE);
  int rc = 0;

  // Base shm file: pages 1-3 populated with BASE_BYTE, page 0 a hole.
  int shm_fd = memfd_create("pagemap-probe-base", 0);
  if (shm_fd < 0)
    return fail("memfd_create");
  if (ftruncate(shm_fd, NPAGES * PS))
    return fail("ftruncate");
  uint8_t *fill = malloc(PS);
  memset(fill, BASE_BYTE, PS);
  for (int i = 1; i < NPAGES; i++)
    if (pwrite(shm_fd, fill, PS, (off_t)i * PS) != PS)
      return fail("base pwrite");

  int pipefd[2];
  if (pipe(pipefd))
    return fail("pipe");

  pid_t child = fork();
  if (child < 0)
    return fail("fork");
  if (child == 0) {
    close(pipefd[0]);
    exit(child_main(shm_fd, pipefd[1]));
  }
  close(pipefd[1]);

  uint64_t base = 0;
  if (read(pipefd[0], &base, sizeof(base)) != sizeof(base)) {
    int st;
    waitpid(child, &st, 0);
    return fail("child never became ready");
  }

  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/pagemap", child);
  int pm_fd = open(path, O_RDONLY);
  if (pm_fd < 0)
    return fail("open pagemap (need root or same-user, CI runs as one user)");

  // T1: classification.
  struct pm p[NPAGES];
  for (int i = 0; i < NPAGES; i++)
    if (read_pagemap(pm_fd, base + (uint64_t)i * PS, &p[i]))
      return fail("pagemap read");
  if (p[0].present && !p[0].file_backed)
    printf("PASS T1a P0 COPY-installed reads as present+anon\n");
  else
    rc |= fail("T1a P0 expected present+anon");
  if (p[1].present && !p[1].file_backed)
    printf("PASS T1b P1 COW-written reads as present+anon\n");
  else
    rc |= fail("T1b P1 expected present+anon");
  if (p[2].present && p[2].file_backed)
    printf("PASS T1c P2 clean CONTINUE page reads as present+file\n");
  else
    rc |= fail("T1c P2 expected present+file");
  if (!p[3].present)
    printf("PASS T1d P3 untouched reads as not-present\n");
  else
    rc |= fail("T1d P3 expected not-present");

  // T2: readv bytes for the three touched classes.
  uint8_t *buf = malloc(PS);
  if (!readv_page(child, base + 0 * PS, buf) && buf[0] == COPY_BYTE)
    printf("PASS T2a P0 readv == COPY pattern\n");
  else
    rc |= fail("T2a P0 readv/copy-bytes");
  if (!readv_page(child, base + 1 * PS, buf) && buf[0] == COW_BYTE)
    printf("PASS T2b P1 readv == COW pattern\n");
  else
    rc |= fail("T2b P1 readv/cow-bytes");
  if (!readv_page(child, base + 2 * PS, buf) && buf[0] == BASE_BYTE)
    printf("PASS T2c P2 readv == base pattern\n");
  else
    rc |= fail("T2c P2 readv/base-bytes");

  // T3: readv on the UNTOUCHED page faults through the child's own
  // handler (MINOR->CONTINUE) and returns base bytes; afterwards the
  // page classifies as file-backed.
  if (!readv_page(child, base + 3 * PS, buf) && buf[0] == BASE_BYTE)
    printf("PASS T3a P3 readv faults through the owner's handler\n");
  else
    rc |= fail("T3a P3 readv-through-fault");
  struct pm p3;
  if (read_pagemap(pm_fd, base + 3 * PS, &p3))
    return fail("pagemap reread");
  if (p3.present && p3.file_backed)
    printf("PASS T3b P3 post-readv classifies as present+file\n");
  else
    rc |= fail("T3b P3 expected present+file after readv");

  kill(child, SIGKILL);
  int st;
  waitpid(child, &st, 0);
  printf(rc ? "RESULT FAIL\n" : "RESULT PASS\n");
  return rc;
}
