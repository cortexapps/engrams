// S0.3 + S0.5 probe: the ADR 0045 substrate under a LIVE KVM guest.
//
// The go/no-go question: does KVM tolerate the substrate's mechanics —
// guest RAM = MAP_SHARED memfd registered UFFD MISSING|MINOR|WP (armed),
// faults resolved with CONTINUE|WP, and first-write carves that mmap a
// per-sandbox overlay page MAP_FIXED *under a live memslot* — with the
// guest seeing correct private values and the shared base staying clean?
//
// Also measured:
//   - does a guest READ carve? (if KVM GUPs FOLL_WRITE for reads, every
//     read would WP-fault -> no sharing -> design problem)
//   - S0.5: KVM dirty log vs the carve set (overlay-as-dirty-set check)
//
// Guest (16-bit real mode @ 0x1000):
//   mov al, [0x5000]      ; read a base page   -> MINOR fault -> CONTINUE|WP
//   out 0x10, al          ; report it (expect 0xAB)
//   mov [0x6000], 0xEE    ; write a base page  -> WP fault -> carve
//   mov al, [0x6000]      ; read back through the carved overlay page
//   out 0x10, al          ; report it (expect 0xEE)
//   mov al, [0x5000]      ; base page again — still clean/shared
//   out 0x10, al          ; report it (expect 0xAB)
//   hlt
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
#ifndef UFFDIO_CONTINUE_MODE_WP
#define UFFDIO_CONTINUE_MODE_WP ((__u64)1 << 1)
#endif

#define PAGE 4096UL
#define MEM_SIZE (2UL << 20)  // 2 MiB
#define NPAGES (MEM_SIZE / PAGE)
#define CODE_GPA 0x1000UL
#define READ_GPA 0x5000UL
#define WRITE_GPA 0x6000UL

static int uffd = -1, base_fd = -1, overlay_fd = -1;
static uint8_t *guest_mem = NULL;  // HVA backing the memslot

static volatile int minor_count = 0, missing_count = 0, wp_count = 0;
static volatile int handler_error = 0;
static volatile uint64_t carved[64];
static volatile int carved_n = 0;

static void die(const char *what) {
  fprintf(stderr, "FATAL: %s: %s\n", what, strerror(errno));
  exit(1);
}

static int uffd_continue_wp(uint64_t addr) {
  struct uffdio_continue c;
  memset(&c, 0, sizeof c);
  c.range.start = addr;
  c.range.len = PAGE;
  c.mode = UFFDIO_CONTINUE_MODE_WP;
  if (ioctl(uffd, UFFDIO_CONTINUE, &c) == -1) return errno;
  return 0;
}

static int uffd_wake(uint64_t addr) {
  struct uffdio_range r = {.start = addr, .len = PAGE};
  if (ioctl(uffd, UFFDIO_WAKE, &r) == -1) return errno;
  return 0;
}

static int carve_page(uint64_t addr) {
  uint64_t off = addr - (uint64_t)guest_mem;
  uint8_t buf[PAGE];
  ssize_t n = pread(base_fd, buf, PAGE, off);
  if (n < 0) return errno;
  if (n < (ssize_t)PAGE) memset(buf + (n > 0 ? n : 0), 0, PAGE - (n > 0 ? n : 0));
  if (pwrite(overlay_fd, buf, PAGE, off) != (ssize_t)PAGE) return errno ? errno : EIO;
  void *p = mmap((void *)addr, PAGE, PROT_READ | PROT_WRITE,
                 MAP_SHARED | MAP_FIXED, overlay_fd, off);
  if (p == MAP_FAILED) return errno;
  if (carved_n < 64) carved[carved_n++] = off;
  return 0;
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
    if (flags & UFFD_PAGEFAULT_FLAG_WP) {
      wp_count++;
      int ce = carve_page(addr);
      if (!ce) ce = uffd_wake(addr);
      if (ce) { handler_error = ce; return NULL; }
    } else if (flags & UFFD_PAGEFAULT_FLAG_MINOR) {
      minor_count++;
      int ce = uffd_continue_wp(addr);
      if (ce && ce != EEXIST) { handler_error = ce; return NULL; }
    } else {
      missing_count++;
      uint64_t off = addr - (uint64_t)guest_mem;
      uint8_t buf[PAGE];
      memset(buf, 0, PAGE);
      if (pwrite(base_fd, buf, PAGE, off) != (ssize_t)PAGE) { handler_error = errno; return NULL; }
      int ce = uffd_continue_wp(addr);
      if (ce && ce != EEXIST) { handler_error = ce; return NULL; }
    }
  }
  return NULL;
}

int main(void) {
  printf("== S0.3/S0.5 kvm substrate probe ==\n");

  // ---- backing files + content ----
  base_fd = memfd_create("s0-kvm-base", MFD_CLOEXEC);
  if (base_fd < 0) die("memfd base");
  if (ftruncate(base_fd, MEM_SIZE)) die("ftruncate base");
  overlay_fd = memfd_create("s0-kvm-overlay", MFD_CLOEXEC);
  if (overlay_fd < 0) die("memfd overlay");
  if (ftruncate(overlay_fd, MEM_SIZE)) die("ftruncate overlay");

  // Guest code (16-bit real mode).
  const uint8_t code[] = {
      0xA0, 0x00, 0x50,             // mov al, [0x5000]
      0xE6, 0x10,                   // out 0x10, al
      0xC6, 0x06, 0x00, 0x60, 0xEE, // mov byte [0x6000], 0xEE
      0xA0, 0x00, 0x60,             // mov al, [0x6000]
      0xE6, 0x10,                   // out 0x10, al
      0xA0, 0x00, 0x50,             // mov al, [0x5000]
      0xE6, 0x10,                   // out 0x10, al
      0xF4,                         // hlt
  };
  if (pwrite(base_fd, code, sizeof code, CODE_GPA) != (ssize_t)sizeof code) die("pwrite code");
  uint8_t ab = 0xAB, z0 = 0x00;
  if (pwrite(base_fd, &ab, 1, READ_GPA) != 1) die("pwrite read page");
  if (pwrite(base_fd, &z0, 1, WRITE_GPA) != 1) die("pwrite write page");
  // Real-mode IVT/page 0 may be touched; give it content too (zeros).
  uint8_t zeros[PAGE] = {0};
  if (pwrite(base_fd, zeros, PAGE, 0) != (ssize_t)PAGE) die("pwrite page0");

  // ---- guest memory mapping ----
  guest_mem = mmap(NULL, MEM_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, base_fd, 0);
  if (guest_mem == MAP_FAILED) die("mmap guest_mem");

  // ---- uffd: register + arm BEFORE the VM touches anything ----
  uffd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK);
  if (uffd < 0) die("userfaultfd");
  struct uffdio_api api;
  memset(&api, 0, sizeof api);
  api.api = UFFD_API;
  api.features = UFFD_FEATURE_MINOR_SHMEM | UFFD_FEATURE_PAGEFAULT_FLAG_WP |
                 UFFD_FEATURE_WP_HUGETLBFS_SHMEM | UFFD_FEATURE_WP_UNPOPULATED;
  if (ioctl(uffd, UFFDIO_API, &api) == -1) die("UFFDIO_API");
  struct uffdio_register reg;
  memset(&reg, 0, sizeof reg);
  reg.range.start = (uint64_t)guest_mem;
  reg.range.len = MEM_SIZE;
  reg.mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_MINOR |
             UFFDIO_REGISTER_MODE_WP;
  if (ioctl(uffd, UFFDIO_REGISTER, &reg) == -1) die("UFFDIO_REGISTER");
  struct uffdio_writeprotect wp;
  memset(&wp, 0, sizeof wp);
  wp.range.start = (uint64_t)guest_mem;
  wp.range.len = MEM_SIZE;
  wp.mode = UFFDIO_WRITEPROTECT_MODE_WP;
  if (ioctl(uffd, UFFDIO_WRITEPROTECT, &wp) == -1) die("UFFDIO_WRITEPROTECT");
  printf("uffd: MISSING|MINOR|WP registered + armed over the future memslot\n");

  pthread_t ht;
  if (pthread_create(&ht, NULL, handler_thread, NULL)) die("pthread handler");

  // ---- KVM setup ----
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
  region.flags = KVM_MEM_LOG_DIRTY_PAGES;  // S0.5 cross-check
  if (ioctl(vm, KVM_SET_USER_MEMORY_REGION, &region) < 0)
    die("KVM_SET_USER_MEMORY_REGION (uffd-armed shm backing)");
  printf("kvm: memslot set over the uffd-armed MAP_SHARED memfd + dirty log on\n");

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

  // ---- run: collect the three OUT values ----
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

  int read_carves_only_writes = 1;
  for (int i = 0; i < carved_n; i++)
    if (carved[i] != WRITE_GPA) read_carves_only_writes = 0;

  uint8_t base_w = 0, ovl_w = 0;
  pread(base_fd, &base_w, 1, WRITE_GPA);
  pread(overlay_fd, &ovl_w, 1, WRITE_GPA);

  printf("guest OUTs: n=%d [%02x %02x %02x] (want ab ee ab)\n", outs_n,
         outs_n > 0 ? outs[0] : 0, outs_n > 1 ? outs[1] : 0, outs_n > 2 ? outs[2] : 0);
  printf("faults: minor=%d missing=%d wp=%d; carved_n=%d (only the written page: %s)\n",
         minor_count, missing_count, wp_count, carved_n,
         read_carves_only_writes ? "yes" : "NO — reads carve too!");
  printf("backing after run: base[0x6000]=0x%02x (clean=0x00) overlay[0x6000]=0x%02x (dirty=0xee)\n",
         base_w, ovl_w);

  // ---- S0.5: KVM dirty log vs the carve set ----
  unsigned long bitmap_bytes = (NPAGES + 63) / 64 * 8;
  uint64_t *bitmap = calloc(1, bitmap_bytes);
  struct kvm_dirty_log dlog;
  memset(&dlog, 0, sizeof dlog);
  dlog.slot = 0;
  dlog.dirty_bitmap = bitmap;
  int dlog_ok = ioctl(vm, KVM_GET_DIRTY_LOG, &dlog) == 0;
  int dirty_n = 0;
  uint64_t dirty_pages[64];
  for (unsigned long pg = 0; pg < NPAGES; pg++)
    if (bitmap[pg / 64] & (1UL << (pg % 64)))
      if (dirty_n < 64) dirty_pages[dirty_n++] = pg * PAGE;
  printf("S0.5: dirty log %s, %d dirty pages:", dlog_ok ? "ok" : "FAILED", dirty_n);
  for (int i = 0; i < dirty_n; i++) printf(" 0x%lx", dirty_pages[i]);
  printf("  | carved:");
  for (int i = 0; i < carved_n; i++) printf(" 0x%lx", (unsigned long)carved[i]);
  printf("\n");

  int pass = halted && outs_n == 3 && outs[0] == 0xAB && outs[1] == 0xEE &&
             outs[2] == 0xAB && base_w != 0xEE && ovl_w == 0xEE &&
             handler_error == 0;
  printf("S0.3 %s: live KVM guest on the substrate (halted=%d handler_error=%d)\n",
         pass ? "PASS" : "FAIL", halted, handler_error);
  return pass ? 0 : 1;
}
