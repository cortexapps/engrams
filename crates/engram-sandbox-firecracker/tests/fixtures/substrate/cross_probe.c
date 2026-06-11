// S0.6 probe: the two facts that decide the fork-v2 design.
//
// The S0.1-S0.5 probes ran the fault handler IN-PROCESS; production's
// engram-uffd-handler is a SEPARATE process. UFFD ioctls cross process
// boundaries through the passed fd, but mmap(MAP_FIXED) carves cannot.
// So:
//
//   v2b (preferred if legal): FC maps the per-template base shm file
//        MAP_PRIVATE and registers MISSING|MINOR. Reads -> minor faults ->
//        handler CONTINUEs against the shared page cache (density!).
//        Writes -> kernel-native COW to anon (privacy, no WP, no carve,
//        no handler involvement, no VMA growth). Dirty tracking stays
//        KVM dirty log; teleport source = process_vm_readv.
//        OPEN QUESTION this probe answers: is MINOR registration +
//        UFFDIO_CONTINUE legal on a MAP_PRIVATE shmem mapping?
//
//   v2a (fallback): MAP_SHARED + WP + overlay carve, with an FC-side
//        carve agent (per-first-write IPC) — only if v2b is EINVAL.
//
// Layout: parent = "FC" (maps, registers, touches pages, COWs);
// child = "handler" (receives uffd fd via SCM_RIGHTS, resolves faults,
// opening the SAME /dev/shm file by path for content fills).
//
// Tests:
//   X1  MINOR|MISSING registration on MAP_PRIVATE-of-shm-file
//   X2  cross-process MINOR fault -> CONTINUE resolves a read
//   X3  cross-process MISSING (hole) -> pwrite-by-path + CONTINUE
//   X4  write to a CONTINUE'd page -> native COW; shm file stays clean;
//       private value survives; second mapping (sibling) still sees base
//   X5  sibling sharing: a second MAP_PRIVATE mapping of the same file
//       reads the same page (page-cache sharing, the density mechanism)
//
// Build:  gcc -O2 -o cross_probe cross_probe.c
// Run:    ./cross_probe

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/userfaultfd.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
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
#define MEM_SIZE (16UL << 20)
#define SHM_PATH "/dev/shm/s06-base"

static void die(const char *what) {
  fprintf(stderr, "FATAL: %s: %s\n", what, strerror(errno));
  exit(1);
}

static int send_fd(int sock, int fd, uint64_t base_addr) {
  struct msghdr msg = {0};
  char cmsgbuf[CMSG_SPACE(sizeof(int))];
  struct iovec io = {.iov_base = &base_addr, .iov_len = sizeof base_addr};
  msg.msg_iov = &io;
  msg.msg_iovlen = 1;
  msg.msg_control = cmsgbuf;
  msg.msg_controllen = sizeof cmsgbuf;
  struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
  c->cmsg_level = SOL_SOCKET;
  c->cmsg_type = SCM_RIGHTS;
  c->cmsg_len = CMSG_LEN(sizeof(int));
  memcpy(CMSG_DATA(c), &fd, sizeof(int));
  return sendmsg(sock, &msg, 0) < 0 ? -1 : 0;
}

static int recv_fd(int sock, int *fd, uint64_t *base_addr) {
  struct msghdr msg = {0};
  char cmsgbuf[CMSG_SPACE(sizeof(int))];
  struct iovec io = {.iov_base = base_addr, .iov_len = sizeof *base_addr};
  msg.msg_iov = &io;
  msg.msg_iovlen = 1;
  msg.msg_control = cmsgbuf;
  msg.msg_controllen = sizeof cmsgbuf;
  if (recvmsg(sock, &msg, 0) <= 0) return -1;
  struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
  if (!c || c->cmsg_type != SCM_RIGHTS) return -1;
  memcpy(fd, CMSG_DATA(c), sizeof(int));
  return 0;
}

// ---- the handler process ----
static int handler_main(int sock) {
  int uffd;
  uint64_t base;
  if (recv_fd(sock, &uffd, &base)) { perror("recv_fd"); return 1; }
  int content_fd = open(SHM_PATH, O_RDWR);
  if (content_fd < 0) { perror("open shm by path"); return 1; }

  for (;;) {
    struct pollfd pfd = {.fd = uffd, .events = POLLIN};
    int pr = poll(&pfd, 1, 5000);
    if (pr <= 0) return pr == 0 ? 0 : 1;  // quiet 5s = parent done
    struct uffd_msg msg;
    ssize_t n = read(uffd, &msg, sizeof msg);
    if (n <= 0) { if (errno == EAGAIN) continue; return 0; }
    if (msg.event != UFFD_EVENT_PAGEFAULT) continue;
    uint64_t addr = msg.arg.pagefault.address & ~(PAGE - 1);
    uint64_t flags = msg.arg.pagefault.flags;
    uint64_t off = addr - base;

    if (!(flags & UFFD_PAGEFAULT_FLAG_MINOR)) {
      // MISSING: fill content by path, then CONTINUE.
      uint8_t buf[PAGE];
      memset(buf, 0xC6, PAGE);
      if (pwrite(content_fd, buf, PAGE, off) != (ssize_t)PAGE) { perror("pwrite"); return 1; }
    }
    struct uffdio_continue c;
    memset(&c, 0, sizeof c);
    c.range.start = addr;
    c.range.len = PAGE;
    if (ioctl(uffd, UFFDIO_CONTINUE, &c) == -1) {
      fprintf(stderr, "HANDLER: UFFDIO_CONTINUE errno=%d (%s) flags=0x%llx\n",
              errno, strerror(errno), (unsigned long long)flags);
      return 2;  // the v2b-killing outcome, distinguishable
    }
  }
}

int main(void) {
  printf("== S0.6 cross-process + MAP_PRIVATE probe ==\n");
  unlink(SHM_PATH);
  int base_fd = open(SHM_PATH, O_RDWR | O_CREAT | O_EXCL, 0600);
  if (base_fd < 0) die("open " SHM_PATH);
  if (ftruncate(base_fd, MEM_SIZE)) die("ftruncate");
  // Populate the first half through the fd (page-cache-present); leave holes after.
  uint8_t page[PAGE];
  for (uint64_t off = 0; off < MEM_SIZE / 2; off += PAGE) {
    memset(page, (int)(0xB0 + (off / PAGE) % 8), PAGE);
    if (pwrite(base_fd, page, PAGE, off) != (ssize_t)PAGE) die("pwrite populate");
  }

  // THE design question: MAP_PRIVATE of the shm file.
  uint8_t *guest = mmap(NULL, MEM_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE, base_fd, 0);
  if (guest == MAP_FAILED) die("mmap MAP_PRIVATE");

  int uffd = (int)syscall(SYS_userfaultfd, O_CLOEXEC | O_NONBLOCK);
  if (uffd < 0) die("userfaultfd");
  struct uffdio_api api;
  memset(&api, 0, sizeof api);
  api.api = UFFD_API;
  api.features = UFFD_FEATURE_MINOR_SHMEM;
  if (ioctl(uffd, UFFDIO_API, &api) == -1) die("UFFDIO_API");

  struct uffdio_register reg;
  memset(&reg, 0, sizeof reg);
  reg.range.start = (uint64_t)guest;
  reg.range.len = MEM_SIZE;
  reg.mode = UFFDIO_REGISTER_MODE_MISSING | UFFDIO_REGISTER_MODE_MINOR;
  if (ioctl(uffd, UFFDIO_REGISTER, &reg) == -1) {
    printf("X1 FAIL: MISSING|MINOR register on MAP_PRIVATE shm: errno=%d (%s)\n",
           errno, strerror(errno));
    printf("VERDICT: v2b dead at registration -> v2a (MAP_SHARED + FC-side carve IPC)\n");
    return 1;
  }
  printf("X1 PASS: MISSING|MINOR registered on MAP_PRIVATE-of-shm-file (ioctls=0x%llx)\n",
         (unsigned long long)reg.ioctls);

  // Spawn the handler process, hand it the uffd.
  int socks[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, socks)) die("socketpair");
  pid_t pid = fork();
  if (pid < 0) die("fork");
  if (pid == 0) {
    close(socks[0]);
    _exit(handler_main(socks[1]));
  }
  close(socks[1]);
  if (send_fd(socks[0], uffd, (uint64_t)guest)) die("send_fd");
  usleep(100 * 1000);  // let the handler set up

  // X2: cross-process MINOR -> CONTINUE on a populated page.
  uint8_t v = guest[3 * PAGE];
  printf("X2 %s: cross-process CONTINUE on MAP_PRIVATE read (val=0x%02x want 0xb3)\n",
         v == 0xB3 ? "PASS" : "FAIL", v);

  // X3: hole -> MISSING -> pwrite-by-path + CONTINUE.
  v = guest[MEM_SIZE / 2 + 5 * PAGE];
  printf("X3 %s: cross-process hole fill (val=0x%02x want 0xc6)\n",
         v == 0xC6 ? "PASS" : "FAIL", v);

  // X4: write -> native COW; file stays clean; private value persists.
  guest[3 * PAGE] = 0xEE;
  uint8_t file_v;
  if (pread(base_fd, &file_v, 1, 3 * PAGE) != 1) die("pread");
  printf("X4 %s: native COW (guest=0x%02x file=0x%02x; want 0xee / 0xb3-clean)\n",
         (guest[3 * PAGE] == 0xEE && file_v == 0xB3) ? "PASS" : "FAIL",
         guest[3 * PAGE], file_v);

  // X5: a sibling MAP_PRIVATE mapping still sees clean base content
  // (page-cache sharing — the density mechanism — and isolation from X4's COW).
  uint8_t *sib = mmap(NULL, MEM_SIZE, PROT_READ, MAP_PRIVATE, base_fd, 0);
  if (sib == MAP_FAILED) die("mmap sibling");
  v = sib[3 * PAGE];
  printf("X5 %s: sibling mapping sees clean base (val=0x%02x want 0xb3)\n",
         v == 0xB3 ? "PASS" : "FAIL", v);

  // X6: session-DIVERGENT pages on resume must install privately —
  // UFFDIO_COPY on this MAP_PRIVATE mapping, never a pwrite into the
  // shared base. Verify COPY works here and the file stays clean.
  {
    uint64_t dv_page = 9 * PAGE;  // populated in the file with 0xB1
    uint8_t divergent[PAGE];
    memset(divergent, 0xD7, PAGE);
    struct uffdio_copy cp;
    memset(&cp, 0, sizeof cp);
    cp.dst = (uint64_t)guest + dv_page;
    cp.src = (uint64_t)divergent;
    cp.len = PAGE;
    int cerr = ioctl(uffd, UFFDIO_COPY, &cp) == -1 ? errno : 0;
    uint8_t fv = 0;
    if (pread(base_fd, &fv, 1, dv_page) != 1) die("pread x6");
    printf("X6 %s: UFFDIO_COPY on MAP_PRIVATE installs divergent page "
           "(errno=%d guest=0x%02x want 0xd7; file=0x%02x clean=0xb1)\n",
           (cerr == 0 && guest[dv_page] == 0xD7 && fv == 0xB1) ? "PASS" : "FAIL",
           cerr, guest[dv_page], fv);
  }

  int st = 0;
  kill(pid, SIGTERM);
  waitpid(pid, &st, 0);
  unlink(SHM_PATH);
  int handler_rc = WIFEXITED(st) ? WEXITSTATUS(st) : 0;
  if (handler_rc == 2)
    printf("VERDICT: CONTINUE EINVAL on MAP_PRIVATE -> v2b dead -> v2a\n");
  printf("== S0.6 done ==\n");
  return 0;
}
