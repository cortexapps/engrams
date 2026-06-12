// ADR 0045 C2: child-side fixture for the Rust dirty-map scanner test.
//
// Stands in for a paused FC guest on the substrate: maps a tmpfs base
// file MAP_PRIVATE, registers UFFD MISSING|MINOR with a handler thread,
// and touches pages into the four states the scanner classifies. The
// PARENT (the Rust test, sitting in the host-agent's seat as our parent
// process) runs `dirty_map::guest_vmas` + `scan_dirty_chunks` against
// our pid and serves chunks via `migrate_peer::read_guest_range`.
//
// Layout: 8 pages, chunk_size = 2 pages → 4 chunks.
//
//   page 0  COPY-installed (MISSING fault on a base hole)   -> dirty
//   page 1  untouched (base hole)                           |  chunk 0 DIRTY
//   page 2  CONTINUE read-only (clean file page)            -> clean
//   page 3  untouched (base populated)                      |  chunk 1 clean
//   page 4  CONTINUE then COW write                         -> dirty
//   page 5  untouched (base populated)                      |  chunk 2 DIRTY
//   page 6  ZERO-copy-installed (MISSING on a base hole)    -> dirty
//   page 7  untouched (base hole; readv faults it through   |  chunk 3 DIRTY
//           our handler, which zero-fills pages >= 6)       |  (all-zero content)
//
// Expected seal bitmap: [1, 0, 1, 1]. Chunk 3's served bytes are all
// zero (the ZeroChunk arm); chunk 0 serves COPY_BYTE+COPY_BYTE; chunk 2
// serves COW_BYTE+BASE_BYTE.
//
// Handler rule: MINOR -> CONTINUE; MISSING -> COPY of COPY_BYTE for
// pages < 6, zeros for pages >= 6.
//
// argv[1] = path to the base file (must live on tmpfs/shmem — UFFD
// MINOR is shmem-only). Prints "READY <map_base_hex>" then parks.

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/userfaultfd.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#define NPAGES 8
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
  uint8_t *map;
};

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
      long page_idx = (long)((addr - (uint64_t)(uintptr_t)a->map) / PS);
      memset(page, page_idx >= 6 ? 0x00 : COPY_BYTE, PS);
      struct uffdio_copy cp = {.dst = addr,
                               .src = (unsigned long)page,
                               .len = (unsigned long)PS,
                               .mode = 0};
      ioctl(a->uffd, UFFDIO_COPY, &cp);
    }
  }
}

int main(int argc, char **argv) {
  if (argc != 2)
    return fail("usage: pagemap_scanner_child <base-file-on-tmpfs>");
  PS = sysconf(_SC_PAGESIZE);

  // Base file: pages 2-5 populated with BASE_BYTE; 0,1,6,7 holes.
  int fd = open(argv[1], O_RDWR | O_CREAT, 0600);
  if (fd < 0)
    return fail("open base file");
  if (ftruncate(fd, NPAGES * PS))
    return fail("ftruncate");
  uint8_t *fill = malloc(PS);
  memset(fill, BASE_BYTE, PS);
  for (int i = 2; i < 6; i++)
    if (pwrite(fd, fill, PS, (off_t)i * PS) != PS)
      return fail("base pwrite");

  uint8_t *map =
      mmap(NULL, NPAGES * PS, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
  if (map == MAP_FAILED)
    return fail("mmap MAP_PRIVATE of base");

  int uffd = syscall(SYS_userfaultfd, O_CLOEXEC);
  if (uffd < 0)
    return fail("userfaultfd");
  struct uffdio_api api = {.api = UFFD_API,
                           .features = UFFD_FEATURE_MINOR_SHMEM};
  if (ioctl(uffd, UFFDIO_API, &api))
    return fail("UFFDIO_API (MINOR_SHMEM)");
  struct uffdio_register reg = {
      .range = {.start = (unsigned long)map, .len = NPAGES * PS},
      .mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_MINOR};
  if (ioctl(uffd, UFFDIO_REGISTER, &reg))
    return fail("UFFDIO_REGISTER MISSING|MINOR");

  struct handler_args a = {.uffd = uffd, .map = map};
  pthread_t th;
  pthread_create(&th, NULL, handler, &a);

  volatile uint8_t sink;
  sink = map[0 * PS];     // page 0: MISSING -> COPY(COPY_BYTE)
  sink = map[2 * PS];     // page 2: MINOR -> CONTINUE (stays clean)
  sink = map[4 * PS];     // page 4: MINOR -> CONTINUE...
  map[4 * PS] = COW_BYTE; //         ...then COW write
  sink = map[6 * PS];     // page 6: MISSING -> COPY(zeros)
  (void)sink;

  printf("READY %lx\n", (unsigned long)(uintptr_t)map);
  fflush(stdout);
  pause();
  return 0;
}
