// S0.3 + S0.5 probe (v2b): the ADR 0045 substrate under a LIVE KVM guest.
//
// v2b design of record (chosen after the S0.6 cross-process probe): guest
// RAM = MAP_PRIVATE of the per-template base shm file, registered UFFD
// MISSING|MINOR (no WP, no carve, no overlay):
//   - base-identical reads -> MINOR fault -> CONTINUE (shared page cache,
//     the density mechanism; MISSING for unpopulated base -> fill by
//     path + CONTINUE)
//   - session-divergent pages -> UFFDIO_COPY (installs private)
//   - guest WRITES -> kernel-native COW; no handler involvement, base file
//     stays clean, KVM dirty log tracks the write
//
// (The earlier MAP_SHARED + WP + overlay-carve variant also passed all
// probes in-process, but the carve's mmap(MAP_FIXED) cannot cross the
// FC/handler process boundary — see uffd_probe.c, kept as the documented
// fallback.)
//
// Guest (16-bit real mode @ 0x1000):
//   mov al, [0x5000]      ; read a base page    -> MINOR -> CONTINUE
//   out 0x10, al          ; report (expect 0xAB)
//   mov al, [0x7000]      ; read a DIVERGENT page -> handler COPYs 0xD7
//   out 0x10, al          ; report (expect 0xD7)
//   mov [0x6000], 0xEE    ; write a base page   -> native COW
//   mov al, [0x6000]      ; read back
//   out 0x10, al          ; report (expect 0xEE)
//   mov al, [0x5000]      ; base page again — still clean/shared
//   out 0x10, al          ; report (expect 0xAB)
//   hlt
//
// PASS = OUTs [ab d7 ee ab], base file clean at 0x6000/0x7000, dirty log
// contains the written page, handler did exactly one COPY (the divergent
// page) and zero COPYs for base reads.
//
// Build:  gcc -O2 -pthread -o kvm_probe kvm_probe.c
// Run:    ./kvm_probe   (needs /dev/kvm)

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/kvm.h>
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
#include <unistd.h>

#ifndef UFFD_FEATURE_MINOR_SHMEM
#define UFFD_FEATURE_MINOR_SHMEM (1 << 10)
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

#define PAGE 4096UL
#define MEM_SIZE (2UL << 20)
#define NPAGES (MEM_SIZE / PAGE)
#define CODE_GPA 0x1000UL
#define READ_GPA 0x5000UL
#define WRITE_GPA 0x6000UL
#define DIVERGENT_GPA 0x7000UL

static int uffd = -1, base_fd = -1;
static uint8_t *guest_mem = NULL;

static volatile int minor_count = 0, missing_count = 0, copy_count = 0;
static volatile int handler_error = 0;

static void die(const char *what) {
  fprintf(stderr, "FATAL: %s: %s\n", what, strerror(errno));
  exit(1);
}

static void *handler_thread(void *arg) {
  (void)arg;
  for (;;) {
    struct pollfd pfd = {.fd = uffd, .events = POLLIN};
    int pr = poll(&pfd, 1, 3000);
    if (pr < 0) { handler_error = errno; return NULL; }
    if (pr == 0) continue;
    struct uffd_msg msg;
    ssize_t n = read(uffd, &msg, sizeof msg);
    if (n <= 0) { if (errno == EAGAIN) continue; handler_error = errno; return NULL; }
    if (msg.event != UFFD_EVENT_PAGEFAULT) continue;
    uint64_t addr = msg.arg.pagefault.address & ~(PAGE - 1);
    uint64_t flags = msg.arg.pagefault.flags;
    uint64_t off = addr - (uint64_t)guest_mem;

    // The real handler consults the session manifest here; the probe's
    // "manifest" is: DIVERGENT_GPA is session-divergent, all else base.
    if (off == DIVERGENT_GPA) {
      uint8_t buf[PAGE];
      memset(buf, 0xD7, PAGE);
      struct uffdio_copy cp;
      memset(&cp, 0, sizeof cp);
      cp.dst = addr;
      cp.src = (uint64_t)buf;
      cp.len = PAGE;
      copy_count++;
      if (ioctl(uffd, UFFDIO_COPY, &cp) == -1 && errno != EEXIST) {
        handler_error = errno;
        return NULL;
      }
      continue;
    }
    if (!(flags & UFFD_PAGEFAULT_FLAG_MINOR)) {
      missing_count++;
      uint8_t buf[PAGE];
      memset(buf, 0, PAGE);
      if (pwrite(base_fd, buf, PAGE, off) != (ssize_t)PAGE) { handler_error = errno; return NULL; }
    } else {
      minor_count++;
    }
    struct uffdio_continue c;
    memset(&c, 0, sizeof c);
    c.range.start = addr;
    c.range.len = PAGE;
    if (ioctl(uffd, UFFDIO_CONTINUE, &c) == -1 && errno != EEXIST) {
      handler_error = errno;
      return NULL;
    }
  }
  return NULL;
}

int main(void) {
  printf("== S0.3/S0.5 kvm substrate probe (v2b: MAP_PRIVATE + COW) ==\n");

  base_fd = memfd_create("s0-kvm-base", MFD_CLOEXEC);
  if (base_fd < 0) die("memfd base");
  if (ftruncate(base_fd, MEM_SIZE)) die("ftruncate base");

  const uint8_t code[] = {
      0xA0, 0x00, 0x50,             // mov al, [0x5000]
      0xE6, 0x10,                   // out 0x10, al
      0xA0, 0x00, 0x70,             // mov al, [0x7000]  (divergent)
      0xE6, 0x10,                   // out 0x10, al
      0xC6, 0x06, 0x00, 0x60, 0xEE, // mov byte [0x6000], 0xEE
      0xA0, 0x00, 0x60,             // mov al, [0x6000]
      0xE6, 0x10,                   // out 0x10, al
      0xA0, 0x00, 0x50,             // mov al, [0x5000]
      0xE6, 0x10,                   // out 0x10, al
      0xF4,                         // hlt
  };
  if (pwrite(base_fd, code, sizeof code, CODE_GPA) != (ssize_t)sizeof code) die("pwrite code");
  uint8_t ab = 0xAB, z0 = 0x00, b9 = 0xB9;
  if (pwrite(base_fd, &ab, 1, READ_GPA) != 1) die("pwrite read page");
  if (pwrite(base_fd, &z0, 1, WRITE_GPA) != 1) die("pwrite write page");
  // The divergent page HAS base content (0xB9) — the handler must shadow it
  // with the session's 0xD7 via COPY, and the file must keep 0xB9.
  if (pwrite(base_fd, &b9, 1, DIVERGENT_GPA) != 1) die("pwrite divergent page");
  uint8_t zeros[PAGE] = {0};
  if (pwrite(base_fd, zeros, PAGE, 0) != (ssize_t)PAGE) die("pwrite page0");

  // v2b: MAP_PRIVATE of the base file.
  guest_mem = mmap(NULL, MEM_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE, base_fd, 0);
  if (guest_mem == MAP_FAILED) die("mmap guest_mem MAP_PRIVATE");

  uffd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK);
  if (uffd < 0) die("userfaultfd");
  struct uffdio_api api;
  memset(&api, 0, sizeof api);
  api.api = UFFD_API;
  api.features = UFFD_FEATURE_MINOR_SHMEM;
  if (ioctl(uffd, UFFDIO_API, &api) == -1) die("UFFDIO_API");
  struct uffdio_register reg;
  memset(&reg, 0, sizeof reg);
  reg.range.start = (uint64_t)guest_mem;
  reg.range.len = MEM_SIZE;
  reg.mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_MINOR;
  if (ioctl(uffd, UFFDIO_REGISTER, &reg) == -1) die("UFFDIO_REGISTER MISSING|MINOR on MAP_PRIVATE");
  printf("uffd: MISSING|MINOR registered on MAP_PRIVATE base over the future memslot\n");

  pthread_t ht;
  if (pthread_create(&ht, NULL, handler_thread, NULL)) die("pthread handler");

  int kvm = open("/dev/kvm", O_RDWR | O_CLOEXEC);
  if (kvm < 0) die("open /dev/kvm");
  int vm = ioctl(kvm, KVM_CREATE_VM, 0UL);
  if (vm < 0) die("KVM_CREATE_VM");
  struct kvm_userspace_memory_region region;
  memset(&region, 0, sizeof region);
  region.slot = 0;
  region.guest_phys_addr = 0;
  region.memory_size = MEM_SIZE;
  region.userspace_addr = (uint64_t)guest_mem;
  region.flags = KVM_MEM_LOG_DIRTY_PAGES;
  if (ioctl(vm, KVM_SET_USER_MEMORY_REGION, &region) < 0)
    die("KVM_SET_USER_MEMORY_REGION (uffd-armed MAP_PRIVATE shm backing)");

  int vcpu = ioctl(vm, KVM_CREATE_VCPU, 0UL);
  if (vcpu < 0) die("KVM_CREATE_VCPU");
  int mmap_size = ioctl(kvm, KVM_GET_VCPU_MMAP_SIZE, 0UL);
  struct kvm_run *run = mmap(NULL, mmap_size, PROT_READ | PROT_WRITE, MAP_SHARED, vcpu, 0);
  if (run == MAP_FAILED) die("mmap kvm_run");

  struct kvm_sregs sregs;
  if (ioctl(vcpu, KVM_GET_SREGS, &sregs) < 0) die("KVM_GET_SREGS");
  sregs.cs.base = 0;
  sregs.cs.selector = 0;
  if (ioctl(vcpu, KVM_SET_SREGS, &sregs) < 0) die("KVM_SET_SREGS");
  struct kvm_regs regs;
  memset(&regs, 0, sizeof regs);
  regs.rip = CODE_GPA;
  regs.rflags = 2;
  if (ioctl(vcpu, KVM_SET_REGS, &regs) < 0) die("KVM_SET_REGS");

  uint8_t outs[8];
  int outs_n = 0;
  int halted = 0;
  for (int iter = 0; iter < 64 && !halted; iter++) {
    if (ioctl(vcpu, KVM_RUN, 0UL) < 0) {
      if (errno == EINTR) continue;
      die("KVM_RUN");
    }
    switch (run->exit_reason) {
      case KVM_EXIT_IO:
        if (run->io.direction == KVM_EXIT_IO_OUT && run->io.port == 0x10 && outs_n < 8)
          outs[outs_n++] = *((uint8_t *)run + run->io.data_offset);
        break;
      case KVM_EXIT_HLT:
        halted = 1;
        break;
      default:
        printf("unexpected exit_reason=%d\n", run->exit_reason);
        halted = 1;
        break;
    }
  }

  uint8_t base_w = 0, base_d = 0;
  pread(base_fd, &base_w, 1, WRITE_GPA);
  pread(base_fd, &base_d, 1, DIVERGENT_GPA);

  printf("guest OUTs: n=%d [%02x %02x %02x %02x] (want ab d7 ee ab)\n", outs_n,
         outs_n > 0 ? outs[0] : 0, outs_n > 1 ? outs[1] : 0,
         outs_n > 2 ? outs[2] : 0, outs_n > 3 ? outs[3] : 0);
  printf("faults: minor=%d missing=%d copies=%d (want copies==1: only the divergent page: %s)\n",
         minor_count, missing_count, copy_count,
         copy_count == 1 ? "yes" : "NO");
  printf("base file after run: [0x6000]=0x%02x (COW-clean=0x00) [0x7000]=0x%02x (shadow-clean=0xb9)\n",
         base_w, base_d);

  // S0.5: dirty log must contain the COW-written page.
  unsigned long bitmap_bytes = (NPAGES + 63) / 64 * 8;
  uint64_t *bitmap = calloc(1, bitmap_bytes);
  struct kvm_dirty_log dlog;
  memset(&dlog, 0, sizeof dlog);
  dlog.slot = 0;
  dlog.dirty_bitmap = bitmap;
  int dlog_ok = ioctl(vm, KVM_GET_DIRTY_LOG, &dlog) == 0;
  int wrote_dirty = dlog_ok &&
      (bitmap[(WRITE_GPA / PAGE) / 64] & (1UL << ((WRITE_GPA / PAGE) % 64)));
  int dirty_n = 0;
  for (unsigned long pg = 0; pg < NPAGES; pg++)
    if (bitmap[pg / 64] & (1UL << (pg % 64))) dirty_n++;
  printf("S0.5 %s: dirty log %s; written page dirty=%d (total dirty=%d)\n",
         (dlog_ok && wrote_dirty) ? "PASS" : "FAIL", dlog_ok ? "ok" : "FAILED",
         wrote_dirty, dirty_n);

  int pass = halted && outs_n == 4 && outs[0] == 0xAB && outs[1] == 0xD7 &&
             outs[2] == 0xEE && outs[3] == 0xAB && base_w != 0xEE &&
             base_d == 0xB9 && copy_count == 1 && dlog_ok && wrote_dirty &&
             handler_error == 0;
  printf("S0.3 %s: live KVM guest on the v2b substrate (halted=%d handler_error=%d)\n",
         pass ? "PASS" : "FAIL", halted, handler_error);
  return pass ? 0 : 1;
}
