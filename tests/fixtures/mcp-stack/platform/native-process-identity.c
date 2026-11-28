// Read-only identity probe for the native fixture host. It never sends signals.
// Build on macOS: cc -std=c11 -Wall -Wextra -Werror -O2 this-file.c -o nativeProbe
#include <libproc.h>
#include <sys/proc_info.h>
#include <sys/types.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--version") == 0) {
        puts("mcp-native-process-identity-v1"); return 0;
    }
    if (argc != 2 || argv[1][0] != '/') return 2;
    size_t root_len = strlen(argv[1]);
    if (root_len < 2 || argv[1][root_len - 1] == '/' || strpbrk(argv[1], "\"\\\n\r\t")) return 2;
    int count = proc_listallpids(NULL, 0);
    if (count <= 0 || count > 1048576) return 3;
    int capacity = count + 1024;
    pid_t *pids = calloc((size_t)capacity, sizeof(pid_t));
    if (!pids) return 3;
    count = proc_listallpids(pids, capacity * (int)sizeof(pid_t));
    if (count < 0 || count >= capacity) { free(pids); return 3; }
    puts("["); int first = 1;
    for (int i = 0; i < count; ++i) {
        if (pids[i] == getpid()) continue;
        struct proc_bsdinfo before, after;
        if (proc_pidinfo(pids[i], PROC_PIDTBSDINFO, 0, &before, sizeof(before)) != sizeof(before)) continue;
        if (before.pbi_uid != getuid()) continue;
        char path[PROC_PIDPATHINFO_MAXSIZE];
        if (proc_pidpath(pids[i], path, sizeof(path)) <= 0) continue;
        if (strncmp(path, argv[1], root_len) || path[root_len] != '/') continue;
        const char *relative = path + root_len;
        size_t path_len = strlen(path);
        if (strcmp(relative, "/assets/hostAgent") && strcmp(relative, "/assets/proxy") && !(strncmp(relative, "/host/", 6) == 0 && path_len > 9 && strcmp(path + path_len - 9, "/netstack") == 0)) continue;
        if (strpbrk(path, "\"\\\n\r\t")) { free(pids); return 4; }
        if (proc_pidinfo(pids[i], PROC_PIDTBSDINFO, 0, &after, sizeof(after)) != sizeof(after)) continue;
        if (before.pbi_start_tvsec != after.pbi_start_tvsec || before.pbi_start_tvusec != after.pbi_start_tvusec || before.pbi_uid != after.pbi_uid) { free(pids); return 4; }
        printf("%s{\"pid\":%u,\"uid\":%u,\"parent\":%u,\"group\":%u,\"seconds\":\"%llu\",\"microseconds\":\"%llu\",\"executable\":\"%s\"}", first ? "" : ",\n", after.pbi_pid, after.pbi_uid, after.pbi_ppid, after.pbi_pgid, (unsigned long long)after.pbi_start_tvsec, (unsigned long long)after.pbi_start_tvusec, path);
        first = 0;
    }
    puts("\n]"); free(pids); return 0;
}
