// S0.1 + S0.2 feasibility probe (v2) for ADR 0045's unified memory substrate.
//
// v1 findings folded in:
//   - fault-around maps cache-present NEIGHBOR pages without faults -> write
//     interception must come from arming UFFDIO_WRITEPROTECT over the whole
//     range upfront (wp_unpopulated), not from install-time WP.
//   - shmem HOLES zero-map silently under MINOR-only registration -> lazy
//     base fill needs MISSING|MINOR|WP triple registration.
//   - sequential carves VMA-merge (32->33 VMAs over 16k carves); the honest
//     worst case is a SCATTERED carve pattern -> measured here.
//
// Tests:
//   T1  register MISSING|MINOR|WP on MAP_SHARED memfd + arm full-range WP
//   T2  read of cache-present page -> MINOR fault -> CONTINUE (read-only ok)
//   T3  read of hole -> MISSING fault -> pwrite+CONTINUE
//   T4  write to CONTINUE'd page -> WP fault -> carve -> writer wakes,
//       base clean / overlay dirty
//   T5  write to fault-around-mapped neighbor (never CONTINUE'd by us) ->
//       still traps (the v1 corruption risk, closed by upfront WP arming)
//   T6  write to a hole -> observe flags -> carve with zero-fill
//   T7  scattered carve: 20k random pages -> us/carve + VMA growth
//   T8  CONTINUE throughput (no pending fault): installs/sec
//
// Build:  gcc -O2 -pthread -o uffd_probe uffd_probe.c
// Run:    ./uffd_probe

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
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
#include <sys/time.h>
#include <unistd.h>

#ifndef UFFD_FEATURE_MINOR_SHMEM
#define UFFD_FEATURE_MINOR_SHMEM (1 << 10)
#endif
#ifndef UFFD_FEATURE_WP_HUGETLBFS_SHMEM
#define UFFD_FEATURE_WP_HUGETLBFS_SHMEM (1 << 12)
#endif
#ifndef UFFD_FEATURE_WP_UNPOPULATED
#define UFFD_FEATURE_WP_UNPOPULATED (1 << 13)
#endif
#ifndef UFFDIO_REGISTER_MODE_MINOR
#define UFFDIO_REGISTER_MODE_MINOR ((__u64)1 << 2)
#endif
#ifndef UFFD_PAGEFAULT_FLAG_MINOR
#define UFFD_PAGEFAULT_FLAG_MINOR (1 << 2)
#endif
#ifndef UFFDIO_CONTINUE
struct uffdio_continue {
  struct uffdio_range range;
  __u64 mode;
  __s64 mapped;
};
#define UFFDIO_CONTINUE _IOWR(UFFDIO, 0x07, struct uffdio_continue)
#endif
#ifndef UFFDIO_CONTINUE_MODE_DONTWAKE
#define UFFDIO_CONTINUE_MODE_DONTWAKE ((__u64)1 << 0)
#endif
#ifndef UFFDIO_CONTINUE_MODE_WP
#define UFFDIO_CONTINUE_MODE_WP ((__u64)1 << 1)
#endif

#define PAGE 4096UL
#define BASE_SIZE (256UL << 20)  // 256 MiB
#define NPAGES (BASE_SIZE / PAGE)
#define SCATTER_N 20000UL

static int uffd = -1;
static int base_fd = -1, overlay_fd = -1;
static uint8_t *guest = NULL;
static uint8_t *populate = NULL;

static volatile uint64_t last_fault_addr = 0;
static volatile uint64_t last_fault_flags = 0;
static volatile int fault_count = 0;
static volatile int minor_count = 0, missing_count = 0, wp_count = 0;
static volatile int wake_errno = -1;
static volatile int handler_error = 0;

static uint64_t now_us(void) {
  struct timeval tv;
  gettimeofday(&tv, NULL);
  return (uint64_t)tv.tv_sec * 1000000 + tv.tv_usec;
}

static int vma_count(void) {
  FILE *f = fopen("/proc/self/maps", "r");
  if (!f) return -1;
  int n = 0;
  char line[512];
  while (fgets(line, sizeof line, f)) n++;
  fclose(f);
  return n;
}

static void die(const char *what) {
  fprintf(stderr, "FATAL: %s: %s\n", what, strerror(errno));
  exit(1);
}

// v2 finding: a plain CONTINUE installs a WRITABLE pte, clearing the wp
// marker — writes then leak into the shared base. Every CONTINUE must
// install write-protected (MODE_WP); writes trap and carve.
static int uffd_continue(uint64_t addr, uint64_t len) {
  struct uffdio_continue c;
  memset(&c, 0, sizeof c);
  c.range.start = addr;
  c.range.len = len;
  c.mode = UFFDIO_CONTINUE_MODE_WP;
  if (ioctl(uffd, UFFDIO_CONTINUE, &c) == -1) return errno;
  return 0;
}

static int uffd_wake(uint64_t addr, uint64_t len) {
  struct uffdio_range r = {.start = addr, .len = len};
  if (ioctl(uffd, UFFDIO_WAKE, &r) == -1) return errno;
  return 0;
}

// Carve an overlay page over `addr`. `have_content`: copy current base bytes
// (pread may hit a hole -> zeros, which is correct for hole-writes).
static int carve_page(uint64_t addr) {
  uint64_t off = addr - (uint64_t)guest;
  uint8_t buf[PAGE];
  ssize_t n = pread(base_fd, buf, PAGE, off);
  if (n < 0) return errno;
  if (n < (ssize_t)PAGE) memset(buf + (n > 0 ? n : 0), 0, PAGE - (n > 0 ? n : 0));
  if (pwrite(overlay_fd, buf, PAGE, off) != (ssize_t)PAGE) return errno ? errno : EIO;
  void *p = mmap((void *)addr, PAGE, PROT_READ | PROT_WRITE,
                 MAP_SHARED | MAP_FIXED, overlay_fd, off);
  if (p == MAP_FAILED) return errno;
  return 0;
}

static void *handler_thread(void *arg) {
  (void)arg;
  for (;;) {
    struct pollfd pfd = {.fd = uffd, .events = POLLIN};
    int pr = poll(&pfd, 1, 2000);
    if (pr < 0) { handler_error = errno; return NULL; }
    if (pr == 0) continue;
    struct uffd_msg msg;
    ssize_t n = read(uffd, &msg, sizeof msg);
    if (n <= 0) { if (errno == EAGAIN) continue; handler_error = errno; return NULL; }
    if (msg.event != UFFD_EVENT_PAGEFAULT) continue;
    uint64_t addr = msg.arg.pagefault.address & ~(PAGE - 1);
    uint64_t flags = msg.arg.pagefault.flags;
    last_fault_addr = addr;
    last_fault_flags = flags;
    fault_count++;

    if (flags & UFFD_PAGEFAULT_FLAG_WP) {
      wp_count++;
      int ce = carve_page(addr);
      if (ce) { handler_error = ce; return NULL; }
      wake_errno = uffd_wake(addr, PAGE);
    } else if (flags & UFFD_PAGEFAULT_FLAG_MINOR) {
      minor_count++;
      int ce = uffd_continue(addr, PAGE);
      if (ce == EEXIST) ce = 0;
      if (ce) { handler_error = ce; return NULL; }
    } else {
      // MISSING-style: page not in cache. Fill content through the fd, then
      // CONTINUE (now cache-present).
      missing_count++;
      uint64_t off = addr - (uint64_t)guest;
      uint8_t buf[PAGE];
      memset(buf, 0xC3, PAGE);
      if (pwrite(base_fd, buf, PAGE, off) != (ssize_t)PAGE) { handler_error = errno; return NULL; }
      int ce = uffd_continue(addr, PAGE);
      if (ce == EEXIST) ce = 0;
      if (ce) { handler_error = ce; return NULL; }
    }
  }
  return NULL;
}

static volatile int writer_done = 0;
static void *writer_thread(void *arg) {
  uint8_t *p = (uint8_t *)arg;
  *p = 0xEE;
  __sync_synchronize();
  writer_done = 1;
  return NULL;
}

// Run a write in a watchdogged thread; returns 1 if it completed.
static int timed_write(uint8_t *p, int timeout_ms) {
  pthread_t wt;
  writer_done = 0;
  if (pthread_create(&wt, NULL, writer_thread, p)) die("pthread writer");
  uint64_t t0 = now_us();
  while (!writer_done && now_us() - t0 < (uint64_t)timeout_ms * 1000) usleep(500);
  if (writer_done) pthread_join(wt, NULL);
  else pthread_detach(wt);
  return writer_done;
}

int main(void) {
  printf("== S0.1/S0.2 uffd substrate probe v2 ==\n");

  base_fd = memfd_create("s0-base", MFD_CLOEXEC);
  if (base_fd < 0) die("memfd_create base");
  if (ftruncate(base_fd, BASE_SIZE)) die("ftruncate base");
  overlay_fd = memfd_create("s0-overlay", MFD_CLOEXEC);
  if (overlay_fd < 0) die("memfd_create overlay");
  if (ftruncate(overlay_fd, BASE_SIZE)) die("ftruncate overlay");

  populate = mmap(NULL, BASE_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, base_fd, 0);
  if (populate == MAP_FAILED) die("mmap populate view");
  // Populate first half; second half stays holes.
  for (uint64_t off = 0; off < BASE_SIZE / 2; off += PAGE)
    populate[off] = (uint8_t)(0xA0 + ((off / PAGE) % 16));

  guest = mmap(NULL, BASE_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, base_fd, 0);
  if (guest == MAP_FAILED) die("mmap guest view");

  uffd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK);
  if (uffd < 0) die("userfaultfd()");
  struct uffdio_api api;
  memset(&api, 0, sizeof api);
  api.api = UFFD_API;
  api.features = UFFD_FEATURE_MINOR_SHMEM | UFFD_FEATURE_PAGEFAULT_FLAG_WP |
                 UFFD_FEATURE_WP_HUGETLBFS_SHMEM | UFFD_FEATURE_WP_UNPOPULATED;
  if (ioctl(uffd, UFFDIO_API, &api) == -1) die("UFFDIO_API");

  // T1: triple registration + full-range WP arming.
  struct uffdio_register reg;
  memset(&reg, 0, sizeof reg);
  reg.range.start = (uint64_t)guest;
  reg.range.len = BASE_SIZE;
  reg.mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_MINOR |
             UFFDIO_REGISTER_MODE_WP;
  if (ioctl(uffd, UFFDIO_REGISTER, &reg) == -1) die("UFFDIO_REGISTER MISSING|MINOR|WP");
  struct uffdio_writeprotect wp;
  memset(&wp, 0, sizeof wp);
  wp.range.start = (uint64_t)guest;
  wp.range.len = BASE_SIZE;
  wp.mode = UFFDIO_WRITEPROTECT_MODE_WP;
  if (ioctl(uffd, UFFDIO_WRITEPROTECT, &wp) == -1) die("UFFDIO_WRITEPROTECT full range");
  printf("T1 PASS: MISSING|MINOR|WP registered + full-range WP armed (ioctls=0x%llx)\n",
         (unsigned long long)reg.ioctls);

  pthread_t ht;
  if (pthread_create(&ht, NULL, handler_thread, NULL)) die("pthread handler");

  // T2: cache-present read -> MINOR -> CONTINUE.
  fault_count = minor_count = missing_count = wp_count = 0;
  uint8_t v = guest[0];
  printf("T2 %s: cached read val=0x%02x faults=%d (minor=%d missing=%d wp=%d) flags=0x%llx\n",
         (v == 0xA0 && minor_count == 1) ? "PASS" : "FAIL", v, fault_count,
         minor_count, missing_count, wp_count, (unsigned long long)last_fault_flags);

  // T3: hole read -> MISSING -> pwrite+CONTINUE.
  fault_count = minor_count = missing_count = wp_count = 0;
  uint64_t hole_off = BASE_SIZE / 2 + 7 * PAGE;
  v = guest[hole_off];
  printf("T3 %s: hole read val=0x%02x (want 0xc3) faults=%d (minor=%d missing=%d wp=%d) flags=0x%llx\n",
         (v == 0xC3 && missing_count >= 1) ? "PASS" : "FAIL", v, fault_count,
         minor_count, missing_count, wp_count, (unsigned long long)last_fault_flags);

  // T4: write to a page we explicitly CONTINUE'd -> WP -> carve -> wake.
  fault_count = wp_count = 0;
  wake_errno = -2;
  uint64_t wp_page = 64 * PAGE;
  v = guest[wp_page];  // ensure mapped (minor fault or fault-around)
  int ok = timed_write(guest + wp_page, 3000);
  uint8_t base_val = 0, overlay_val = 0;
  if (pread(base_fd, &base_val, 1, wp_page) != 1) base_val = 1;
  if (pread(overlay_fd, &overlay_val, 1, wp_page) != 1) overlay_val = 1;
  printf("T4 %s: WP write -> carve (writer_done=%d wp_faults=%d wake_errno=%d) "
         "base=0x%02x overlay=0x%02x guest=0x%02x\n",
         (ok && wp_count >= 1 && base_val != 0xEE && overlay_val == 0xEE &&
          guest[wp_page] == 0xEE) ? "PASS" : "FAIL",
         ok, wp_count, wake_errno, base_val, overlay_val, guest[wp_page]);

  // T5: write to a fault-around-mapped NEIGHBOR (we never CONTINUE'd it).
  // v1's corruption risk: without upfront WP arming this write would land in
  // the shared base unintercepted.
  fault_count = wp_count = 0;
  uint64_t nb_page = 65 * PAGE;  // mapped by fault-around of T4's read, likely
  ok = timed_write(guest + nb_page, 3000);
  pread(base_fd, &base_val, 1, nb_page);
  pread(overlay_fd, &overlay_val, 1, nb_page);
  printf("T5 %s: neighbor write trapped (writer_done=%d wp_faults=%d) "
         "base=0x%02x overlay=0x%02x\n",
         (ok && wp_count >= 1 && base_val != 0xEE && overlay_val == 0xEE) ? "PASS" : "FAIL",
         ok, wp_count, base_val, overlay_val);

  // T6: write straight to a HOLE (never read) -> observe flag sequence.
  fault_count = minor_count = missing_count = wp_count = 0;
  uint64_t hw_page = BASE_SIZE / 2 + 100 * PAGE;
  ok = timed_write(guest + hw_page, 3000);
  pread(overlay_fd, &overlay_val, 1, hw_page);
  printf("T6 %s: hole write (writer_done=%d minor=%d missing=%d wp=%d) overlay=0x%02x\n",
         (ok && overlay_val == 0xEE) ? "PASS" : "FAIL",
         ok, minor_count, missing_count, wp_count, overlay_val);

  // T7: scattered carve — worst-case VMA growth + cost.
  // Use a fresh region in the populated half: pages [1024, 1024+65536).
  uint64_t span = 65536;  // 256 MiB / 4 KiB = 65536 total; use first-half span
  uint64_t start_pg = 1024;
  uint64_t span_pg = (NPAGES / 2) - start_pg - 1;
  // Pre-map everything in the span via CONTINUE (no faults — direct install).
  uint64_t t0 = now_us();
  uint64_t installed = 0;
  for (uint64_t i = 0; i < span_pg; i++) {
    int ce = uffd_continue((uint64_t)guest + (start_pg + i) * PAGE, PAGE);
    if (ce && ce != EEXIST) { printf("T7 pre-CONTINUE err=%d at %lu\n", ce, i); break; }
    installed++;
  }
  uint64_t t_cont = now_us() - t0;
  printf("T8 INFO: %lu direct CONTINUEs in %.1f ms (%.2f us/page, %.1f GiB/s effective)\n",
         installed, t_cont / 1000.0, (double)t_cont / installed,
         installed * PAGE / (t_cont / 1e6) / (1 << 30));

  // Scatter: LCG over the span, carve 20k distinct pages.
  int vmas_before = vma_count();
  uint64_t x = 12345;
  t0 = now_us();
  uint64_t carved = 0;
  for (uint64_t i = 0; i < SCATTER_N; i++) {
    x = (x * 6364136223846793005ULL + 1442695040888963407ULL);
    uint64_t pg = start_pg + (x >> 33) % span_pg;
    int ce = carve_page((uint64_t)guest + pg * PAGE);
    if (ce) { printf("T7 carve err=%d (%s) at %lu, vmas=%d\n", ce, strerror(ce), i, vma_count()); break; }
    carved++;
  }
  uint64_t dt = now_us() - t0;
  int vmas_after = vma_count();
  printf("T7 INFO: %lu scattered carves in %.1f ms (%.2f us/carve); VMAs %d -> %d "
         "(+%.2f/carve; max_map_count=%d)\n",
         carved, dt / 1000.0, (double)dt / carved, vmas_before, vmas_after,
         (double)(vmas_after - vmas_before) / carved, 65530);

  // T9: per-fault ROUND-TRIP cost (fault -> poll -> CONTINUE|WP -> wake) —
  // what a resume fault-storm actually pays. Fresh view of the same base,
  // registered + armed, then sequential reads through real faults.
  {
    uint8_t *g2 = mmap(NULL, BASE_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, base_fd, 0);
    if (g2 == MAP_FAILED) die("mmap g2");
    struct uffdio_register r2;
    memset(&r2, 0, sizeof r2);
    r2.range.start = (uint64_t)g2;
    r2.range.len = BASE_SIZE;
    r2.mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_MINOR |
              UFFDIO_REGISTER_MODE_WP;
    if (ioctl(uffd, UFFDIO_REGISTER, &r2) == -1) die("register g2");
    struct uffdio_writeprotect w2;
    memset(&w2, 0, sizeof w2);
    w2.range.start = (uint64_t)g2;
    w2.range.len = BASE_SIZE;
    w2.mode = UFFDIO_WRITEPROTECT_MODE_WP;
    if (ioctl(uffd, UFFDIO_WRITEPROTECT, &w2) == -1) die("wp g2");
    // NOTE: handler resolves faults against `guest` addressing; generalize by
    // observing addr directly (handler uses fault addr, so it's fine — the
    // base_fd offset math in MISSING/carve uses `guest`; stay in the cached
    // half so only MINOR faults fire and no offset math is needed).
    int before = fault_count;
    uint64_t t9 = now_us();
    uint64_t n9 = 10000;
    for (uint64_t i = 0; i < n9; i++) {
      volatile uint8_t x = g2[(1 + i) * PAGE];
      (void)x;
    }
    uint64_t dt9 = now_us() - t9;
    printf("T9 INFO: %lu faulted reads in %.1f ms (%.2f us/fault round-trip, "
           "%d faults observed)\n",
           n9, dt9 / 1000.0, (double)dt9 / n9, fault_count - before);
  }

  printf("== probe v2 done (handler_error=%d, faults total=%d) ==\n",
         handler_error, fault_count);
  return handler_error ? 1 : 0;
}
