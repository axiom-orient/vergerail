/*
 * Vergerail's macOS process-custody boundary.
 *
 * This helper is deliberately a small C boundary: Rust keeps unsafe_code
 * forbidden, while this file owns the fork/exec, kqueue, and libproc calls
 * needed to keep the Codex leader unreaped until its private process group has
 * been scanned and torn down.
 */
#define _DARWIN_C_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <libproc.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/event.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define EXIT_GUARDIAN_FAILURE 70
#define EXIT_USAGE_FAILURE 64
#define EXIT_UNSUPPORTED 78
#define MAX_GROUP_MEMBERS 4096
#define TERM_GRACE_MS 1000
#define SCAN_DELAY_MS 50
#define SCAN_ATTEMPTS 24

static volatile sig_atomic_t stop_requested = 0;
static volatile sig_atomic_t worker_pid = -1;
static volatile sig_atomic_t liveness_fd = -1;

static void request_stop(int signal_number) {
    (void)signal_number;
    stop_requested = 1;
    if (worker_pid > 0) {
        (void)kill((pid_t)worker_pid, SIGTERM);
    }
    if (liveness_fd >= 0) {
        (void)close((int)liveness_fd);
        liveness_fd = -1;
    }
}

static int set_cloexec(int fd) {
    int flags = fcntl(fd, F_GETFD, 0);
    if (flags < 0 || fcntl(fd, F_SETFD, flags | FD_CLOEXEC) < 0) {
        return -1;
    }
    return 0;
}

static int set_nonblocking(int fd) {
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0) {
        return -1;
    }
    return 0;
}

static int sleep_milliseconds(long milliseconds) {
    struct timespec requested;
    requested.tv_sec = milliseconds / 1000;
    requested.tv_nsec = (milliseconds % 1000) * 1000000L;
    while (nanosleep(&requested, &requested) < 0) {
        if (errno != EINTR) {
            return -1;
        }
    }
    return 0;
}

/* Return 0 when the private pgrp contains only the known unreaped leader. */
static int group_has_no_other_member(pid_t leader, int *has_other) {
    pid_t members[MAX_GROUP_MEMBERS];
    int bytes = (int)sizeof(members);
    int count = proc_listpgrppids(leader, members, bytes);
    if (count < 0) {
        return -1;
    }
    if (count > MAX_GROUP_MEMBERS) {
        errno = EOVERFLOW;
        return -1;
    }

    *has_other = 0;
    for (int index = 0; index < count; index += 1) {
        if (members[index] != leader) {
            *has_other = 1;
            break;
        }
    }
    return 0;
}

/* Observe without reaping.  The leader remains the custody anchor. */
static int observe_leader(pid_t leader, int *exited, int *status) {
    siginfo_t info;
    memset(&info, 0, sizeof(info));
    int result;
    do {
        result = waitid(P_PID, leader, &info, WEXITED | WNOHANG | WNOWAIT);
    } while (result < 0 && errno == EINTR);
    if (result < 0) {
        return -1;
    }
    *exited = 0;
    if (info.si_pid != leader) {
        return 0;
    }
    if (info.si_code == CLD_EXITED) {
        *status = (info.si_status & 0xff) << 8;
    } else if (info.si_code == CLD_KILLED || info.si_code == CLD_DUMPED) {
        *status = info.si_status & 0x7f;
    } else {
        errno = EPROTO;
        return -1;
    }
    *exited = 1;
    return 0;
}
/*
 * The leader is intentionally still waitable here.  No signal is sent after
 * waitpid reaps it; this is the property that prevents a stale numeric PGID
 * from becoming a signal target after PID/PGID reuse.
 */
static int signal_private_group(pid_t leader, int signal_number, int leader_exited) {
    pid_t group = getpgid(leader);
    if (group < 0) {
        if (errno != ESRCH) {
            return -1;
        }
        if (!leader_exited) {
            errno = ESRCH;
            return -1;
        }
        /*
         * A zombie leader no longer answers getpgid, but its unreaped PID is
         * still the immutable pgrp anchor.  Signal that group only when the
         * libproc scan proves that another member is present.
         */
        int has_other = 0;
        if (group_has_no_other_member(leader, &has_other) < 0) {
            return -1;
        }
        if (!has_other) {
            return 0;
        }
        if (killpg(leader, signal_number) < 0 && errno != ESRCH) {
            return -1;
        }
        return 0;
    }
    if (group != leader) {
        errno = EPERM;
        return -1;
    }
    if (killpg(leader, signal_number) < 0 && errno != ESRCH) {
        return -1;
    }
    return 0;
}

static int wait_for_leader_exit(pid_t leader, int *status, int force_kill) {
    int exited = 0;
    if (observe_leader(leader, &exited, status) < 0) {
        return -1;
    }
    if (exited) {
        return 0;
    }
    if (signal_private_group(leader, force_kill ? SIGKILL : SIGTERM, 0) < 0) {
        return -1;
    }
    for (int attempt = 0; attempt < SCAN_ATTEMPTS; attempt += 1) {
        if (observe_leader(leader, &exited, status) < 0) {
            return -1;
        }
        if (exited) {
            return 0;
        }
        if (sleep_milliseconds(SCAN_DELAY_MS) < 0) {
            return -1;
        }
    }
    errno = ETIMEDOUT;
    return 1;
}

/*
 * The leader must be observed exited before an empty member scan can mean
 * anything.  Once it is exited, require an immediate and a delayed empty
 * scan while WNOWAIT still holds the leader anchor.  A survivor forces KILL;
 * every scan, timeout, and signal error remains a typed guardian failure.
 */
static int terminate_private_group(pid_t leader, int *status) {
    int leader_exit_result = wait_for_leader_exit(leader, status, 0);
    if (leader_exit_result < 0) {
        return -1;
    }
    if (leader_exit_result > 0) {
        leader_exit_result = wait_for_leader_exit(leader, status, 1);
        if (leader_exit_result != 0) {
            return -1;
        }
    }

    for (int round = 0; round < SCAN_ATTEMPTS; round += 1) {
        int exited = 0;
        if (observe_leader(leader, &exited, status) < 0 || !exited) {
            if (!exited) {
                errno = EAGAIN;
            }
            return -1;
        }

        int has_other = 0;
        if (group_has_no_other_member(leader, &has_other) < 0) {
            return -1;
        }
        if (has_other) {
            if (signal_private_group(leader, SIGKILL, 1) < 0) {
                return -1;
            }
        } else {
            if (sleep_milliseconds(SCAN_DELAY_MS) < 0) {
                return -1;
            }
            if (group_has_no_other_member(leader, &has_other) < 0) {
                return -1;
            }
            if (!has_other) {
                if (observe_leader(leader, &exited, status) < 0 || !exited) {
                    if (!exited && errno == 0) {
                        errno = EAGAIN;
                    }
                    return -1;
                }
                return 0;
            }
            if (signal_private_group(leader, SIGKILL, 1) < 0) {
                return -1;
            }
        }
        if (sleep_milliseconds(SCAN_DELAY_MS) < 0) {
            return -1;
        }
    }
    errno = ETIMEDOUT;
    return -1;
}

static int reap_leader(pid_t leader, int *status) {
    pid_t result;
    do {
        result = waitpid(leader, status, WNOHANG);
    } while (result < 0 && errno == EINTR);
    if (result == leader) {
        return 0;
    }
    if (result == 0) {
        errno = EAGAIN;
    }
    return -1;
}

static int read_exec_result(int fd, int *child_errno) {
    unsigned char *cursor = (unsigned char *)child_errno;
    size_t received = 0;
    for (;;) {
        ssize_t count = read(fd, cursor + received, sizeof(*child_errno) - received);
        if (count == 0) {
            return received == 0 ? 0 : -1;
        }
        if (count < 0) {
            if (errno == EINTR) {
                continue;
            }
            return -1;
        }
        received += (size_t)count;
        if (received == sizeof(*child_errno)) {
            return 1;
        }
    }
}

static int register_parent_watcher(int queue, pid_t parent_pid) {
    struct kevent event;
    EV_SET(&event, parent_pid, EVFILT_PROC, EV_ADD | EV_ONESHOT, NOTE_EXIT, 0, NULL);
    return kevent(queue, &event, 1, NULL, 0, NULL);
}

static int parent_exited(int queue, int monitor_liveness_fd) {
    struct pollfd liveness = {
        .fd = monitor_liveness_fd,
        .events = POLLIN | POLLHUP | POLLERR,
        .revents = 0,
    };
    int liveness_count;
    do {
        liveness_count = poll(&liveness, 1, 0);
    } while (liveness_count < 0 && errno == EINTR);
    if (liveness_count < 0 || (liveness.revents & (POLLHUP | POLLERR | POLLNVAL)) != 0) {
        return -1;
    }
    if ((liveness.revents & POLLIN) != 0) {
        unsigned char byte;
        ssize_t count = read(monitor_liveness_fd, &byte, sizeof(byte));
        if (count == 0) {
            return -1;
        }
        if (count < 0 && errno != EAGAIN && errno != EINTR) {
            return -1;
        }
    }

    struct kevent event;
    struct timespec immediate = {0, 0};
    int count = kevent(queue, NULL, 0, &event, 1, &immediate);
    if (count < 0) {
        return -1;
    }
    return count > 0 ? 1 : 0;
}

static int run_worker(pid_t parent_pid, int monitor_liveness_fd, char **arguments) {
    if (set_nonblocking(monitor_liveness_fd) < 0) {
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }
    if (setsid() < 0) {
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }

    int queue = kqueue();
    if (queue < 0 || set_cloexec(queue) < 0) {
        if (queue >= 0) {
            close(queue);
        }
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }
    if (register_parent_watcher(queue, parent_pid) < 0) {
        close(queue);
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }

    int exec_pipe[2] = {-1, -1};
    if (pipe(exec_pipe) < 0 || set_cloexec(exec_pipe[0]) < 0 || set_cloexec(exec_pipe[1]) < 0) {
        close(exec_pipe[0]);
        close(exec_pipe[1]);
        close(queue);
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }

    pid_t leader = fork();
    if (leader < 0) {
        close(exec_pipe[0]);
        close(exec_pipe[1]);
        close(queue);
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }
    if (leader == 0) {
        int child_errno = 0;
        close(exec_pipe[0]);
        if (setpgid(0, 0) < 0) {
            child_errno = errno;
            (void)write(exec_pipe[1], &child_errno, sizeof(child_errno));
            _exit(127);
        }
        execv(arguments[0], arguments);
        child_errno = errno;
        (void)write(exec_pipe[1], &child_errno, sizeof(child_errno));
        _exit(127);
    }

    close(exec_pipe[1]);
    int child_errno = 0;
    int exec_result = read_exec_result(exec_pipe[0], &child_errno);
    close(exec_pipe[0]);
    if (exec_result < 0) {
        int status = 0;
        if (terminate_private_group(leader, &status) < 0 || reap_leader(leader, &status) < 0) {
            close(queue);
            close(monitor_liveness_fd);
            return EXIT_GUARDIAN_FAILURE;
        }
        close(queue);
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }
    if (exec_result > 0) {
        int status = 0;
        if (terminate_private_group(leader, &status) < 0 || reap_leader(leader, &status) < 0) {
            close(queue);
            close(monitor_liveness_fd);
            return EXIT_GUARDIAN_FAILURE;
        }
        close(queue);
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }

    /* After EOF, exec is confirmed and this guardian no longer owns stdio. */
    close(STDIN_FILENO);
    close(STDOUT_FILENO);
    close(STDERR_FILENO);

    int status = 0;
    for (;;) {
        siginfo_t info;
        memset(&info, 0, sizeof(info));
        if (waitid(P_PID, leader, &info, WEXITED | WNOHANG | WNOWAIT) < 0) {
            break;
        }
        if (info.si_pid == leader) {
            status = 0;
            if (info.si_code == CLD_EXITED) {
                status = (info.si_status & 0xff) << 8;
            } else if (info.si_code == CLD_KILLED || info.si_code == CLD_DUMPED) {
                status = info.si_status & 0x7f;
            }
            break;
        }
        if (stop_requested) {
            break;
        }
        int parent_status = parent_exited(queue, monitor_liveness_fd);
        if (parent_status < 0 || parent_status > 0) {
            break;
        }
        if (sleep_milliseconds(SCAN_DELAY_MS) < 0) {
            break;
        }
    }

    if (terminate_private_group(leader, &status) < 0) {
        /* The leader remains unreaped; no stale-group fallback is possible. */
        close(queue);
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }
    if (reap_leader(leader, &status) < 0) {
        close(queue);
        close(monitor_liveness_fd);
        return EXIT_GUARDIAN_FAILURE;
    }
    close(queue);
    close(monitor_liveness_fd);
    if (WIFEXITED(status)) {
        return WEXITSTATUS(status);
    }
    if (WIFSIGNALED(status)) {
        return 128 + WTERMSIG(status);
    }
    return EXIT_GUARDIAN_FAILURE;
}

int main(int argc, char **argv) {
#if !defined(__APPLE__) || !defined(__aarch64__)
    (void)argc;
    (void)argv;
    return EXIT_UNSUPPORTED;
#else
    char **arguments = NULL;
    if (argc < 3 || strcmp(argv[1], "--") != 0) {
        return EXIT_USAGE_FAILURE;
    }
    arguments = &argv[2];

    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = request_stop;
    sigemptyset(&action.sa_mask);
    action.sa_flags = 0;
    if (sigaction(SIGTERM, &action, NULL) < 0 || sigaction(SIGINT, &action, NULL) < 0 ||
        sigaction(SIGHUP, &action, NULL) < 0) {
        return EXIT_GUARDIAN_FAILURE;
    }
    (void)signal(SIGPIPE, SIG_IGN);

    pid_t parent_pid = getppid();
    int monitor_liveness_pipe[2] = {-1, -1};
    if (pipe(monitor_liveness_pipe) < 0 || set_cloexec(monitor_liveness_pipe[0]) < 0 ||
        set_cloexec(monitor_liveness_pipe[1]) < 0) {
        if (monitor_liveness_pipe[0] >= 0) {
            close(monitor_liveness_pipe[0]);
        }
        if (monitor_liveness_pipe[1] >= 0) {
            close(monitor_liveness_pipe[1]);
        }
        return EXIT_GUARDIAN_FAILURE;
    }
    pid_t worker = fork();
    if (worker < 0) {
        close(monitor_liveness_pipe[0]);
        close(monitor_liveness_pipe[1]);
        return EXIT_GUARDIAN_FAILURE;
    }
    if (worker == 0) {
        close(monitor_liveness_pipe[1]);
        int result = run_worker(parent_pid, monitor_liveness_pipe[0], arguments);
        _exit(result);
    }

    close(monitor_liveness_pipe[0]);
    worker_pid = worker;
    liveness_fd = monitor_liveness_pipe[1];
    close(STDIN_FILENO);
    close(STDOUT_FILENO);
    close(STDERR_FILENO);
    int status = 0;
    while (waitpid(worker, &status, 0) < 0) {
        if (errno != EINTR) {
            return EXIT_GUARDIAN_FAILURE;
        }
    }
    worker_pid = -1;
    close((int)liveness_fd);
    liveness_fd = -1;
    if (WIFEXITED(status)) {
        return WEXITSTATUS(status);
    }
    return EXIT_GUARDIAN_FAILURE;
#endif
}
