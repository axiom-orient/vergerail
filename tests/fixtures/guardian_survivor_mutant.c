/*
 * Test-only mutation fixture.  This file is intentionally outside the
 * production build input: it models the retired first-empty-scan behavior so
 * the regression test can prove that a late fork would have survived it.
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
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define EXIT_FAILURE_GUARDIAN 70
#define EXIT_USAGE 64
#define MAX_GROUP_MEMBERS 4096
#define POLL_DELAY_MS 10
#define SCAN_ATTEMPTS 24
#define SELF_TIMEOUT_SECONDS 15

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

static int sleep_milliseconds(long milliseconds) {
    struct timespec requested = {
        .tv_sec = milliseconds / 1000,
        .tv_nsec = (milliseconds % 1000) * 1000000L,
    };
    while (nanosleep(&requested, &requested) < 0) {
        if (errno != EINTR) {
            return -1;
        }
    }
    return 0;
}

static int path_exists(const char *path) {
    return access(path, F_OK) == 0;
}

static int group_has_other_member(pid_t leader, int *has_other) {
    pid_t members[MAX_GROUP_MEMBERS];
    int count = proc_listpgrppids(leader, members, (int)sizeof(members));
    if (count < 0 || count > MAX_GROUP_MEMBERS) {
        return -1;
    }
    *has_other = 0;
    for (int index = 0; index < count; index += 1) {
        if (members[index] != leader) {
            *has_other = 1;
            return 0;
        }
    }
    return 0;
}

static int poll_parent_liveness(int fd) {
    struct pollfd event = {
        .fd = fd,
        .events = POLLIN | POLLHUP | POLLERR,
        .revents = 0,
    };
    int result;
    do {
        result = poll(&event, 1, 0);
    } while (result < 0 && errno == EINTR);
    if (result < 0) {
        return -1;
    }
    if ((event.revents & (POLLHUP | POLLERR | POLLNVAL)) != 0) {
        return 1;
    }
    if ((event.revents & POLLIN) != 0) {
        unsigned char byte;
        ssize_t count = read(fd, &byte, sizeof(byte));
        if (count == 0) {
            return 1;
        }
        if (count < 0 && errno != EINTR && errno != EAGAIN) {
            return -1;
        }
    }
    return 0;
}

static int write_ack(const char *path) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) {
        return -1;
    }
    const char acknowledgement[] = "first-empty-scan\n";
    ssize_t written = write(fd, acknowledgement, sizeof(acknowledgement) - 1);
    int saved_errno = errno;
    if (close(fd) < 0 && written >= 0) {
        written = -1;
        saved_errno = errno;
    }
    if (written != (ssize_t)(sizeof(acknowledgement) - 1)) {
        errno = saved_errno;
        return -1;
    }
    return 0;
}

static int reap_leader(pid_t leader, int *status) {
    pid_t result;
    do {
        result = waitpid(leader, status, 0);
    } while (result < 0 && errno == EINTR);
    return result == leader ? 0 : -1;
}

/* This is the intentionally retired behavior used only by the mutation test. */
static int legacy_cleanup(pid_t leader, const char *ack_path, const char *ready_path,
                          int liveness) {
    if (killpg(leader, SIGTERM) < 0 && errno != ESRCH) {
        return -1;
    }

    /* The fixture's leader writes this gate before it can fork the late child. */
    for (int attempt = 0; attempt < SCAN_ATTEMPTS; attempt += 1) {
        if (path_exists(ready_path)) {
            break;
        }
        int parent_status = poll_parent_liveness(liveness);
        if (parent_status < 0) {
            return -1;
        }
        if (sleep_milliseconds(POLL_DELAY_MS) < 0) {
            return -1;
        }
    }
    if (!path_exists(ready_path)) {
        (void)killpg(leader, SIGKILL);
        return -1;
    }

    /* Legacy bug: an empty scan before leader exit is treated as success. */
    for (int attempt = 0; attempt < SCAN_ATTEMPTS; attempt += 1) {
        int has_other = 0;
        if (group_has_other_member(leader, &has_other) < 0) {
            return -1;
        }
        if (!has_other) {
            return write_ack(ack_path);
        }
        if (sleep_milliseconds(POLL_DELAY_MS) < 0) {
            return -1;
        }
    }
    (void)killpg(leader, SIGKILL);
    return -1;
}

static int run_worker(int liveness, const char *ack_path, const char *ready_path,
                      char **arguments) {
    (void)alarm(SELF_TIMEOUT_SECONDS);
    if (setsid() < 0) {
        close(liveness);
        return EXIT_FAILURE_GUARDIAN;
    }

    int exec_pipe[2] = {-1, -1};
    if (pipe(exec_pipe) < 0 || set_cloexec(exec_pipe[0]) < 0 || set_cloexec(exec_pipe[1]) < 0) {
        close(exec_pipe[0]);
        close(exec_pipe[1]);
        close(liveness);
        return EXIT_FAILURE_GUARDIAN;
    }
    pid_t leader = fork();
    if (leader < 0) {
        close(exec_pipe[0]);
        close(exec_pipe[1]);
        close(liveness);
        return EXIT_FAILURE_GUARDIAN;
    }
    if (leader == 0) {
        close(exec_pipe[0]);
        if (setpgid(0, 0) < 0) {
            int child_errno = errno;
            (void)write(exec_pipe[1], &child_errno, sizeof(child_errno));
            _exit(127);
        }
        execv(arguments[0], arguments);
        int child_errno = errno;
        (void)write(exec_pipe[1], &child_errno, sizeof(child_errno));
        _exit(127);
    }

    close(exec_pipe[1]);
    unsigned char byte;
    ssize_t exec_read;
    do {
        exec_read = read(exec_pipe[0], &byte, sizeof(byte));
    } while (exec_read < 0 && errno == EINTR);
    close(exec_pipe[0]);
    if (exec_read > 0) {
        (void)killpg(leader, SIGKILL);
        int status = 0;
        (void)reap_leader(leader, &status);
        close(liveness);
        return EXIT_FAILURE_GUARDIAN;
    }

    close(STDIN_FILENO);
    close(STDOUT_FILENO);
    close(STDERR_FILENO);
    for (;;) {
        int status = 0;
        pid_t result = waitpid(leader, &status, WNOHANG);
        if (result == leader) {
            close(liveness);
            return WIFEXITED(status) ? WEXITSTATUS(status) : EXIT_FAILURE_GUARDIAN;
        }
        if (result < 0 && errno != EINTR) {
            close(liveness);
            return EXIT_FAILURE_GUARDIAN;
        }
        int parent_status = poll_parent_liveness(liveness);
        if (parent_status < 0) {
            stop_requested = 1;
        } else if (parent_status > 0) {
            stop_requested = 1;
        }
        if (stop_requested) {
            int cleanup_status = legacy_cleanup(leader, ack_path, ready_path, liveness);
            int status_after_cleanup = 0;
            int reap_status = reap_leader(leader, &status_after_cleanup);
            close(liveness);
            if (cleanup_status < 0 || reap_status < 0) {
                return EXIT_FAILURE_GUARDIAN;
            }
            return WIFEXITED(status_after_cleanup) ? WEXITSTATUS(status_after_cleanup)
                                                   : EXIT_FAILURE_GUARDIAN;
        }
        if (sleep_milliseconds(POLL_DELAY_MS) < 0) {
            close(liveness);
            return EXIT_FAILURE_GUARDIAN;
        }
    }
}

int main(int argc, char **argv) {
    if (argc < 8 || strcmp(argv[1], "--survivor-probe") != 0 || strcmp(argv[3], "--wait-for") != 0 ||
        strcmp(argv[5], "--") != 0) {
        return EXIT_USAGE;
    }
    const char *ack_path = argv[2];
    const char *ready_path = argv[4];
    char **arguments = &argv[6];
    (void)alarm(SELF_TIMEOUT_SECONDS);

    struct sigaction action;
    memset(&action, 0, sizeof(action));
    action.sa_handler = request_stop;
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGTERM, &action, NULL) < 0 || sigaction(SIGINT, &action, NULL) < 0 ||
        sigaction(SIGHUP, &action, NULL) < 0) {
        return EXIT_FAILURE_GUARDIAN;
    }
    (void)signal(SIGPIPE, SIG_IGN);

    int liveness_pipe[2] = {-1, -1};
    if (pipe(liveness_pipe) < 0 || set_cloexec(liveness_pipe[0]) < 0 ||
        set_cloexec(liveness_pipe[1]) < 0) {
        close(liveness_pipe[0]);
        close(liveness_pipe[1]);
        return EXIT_FAILURE_GUARDIAN;
    }
    pid_t worker = fork();
    if (worker < 0) {
        close(liveness_pipe[0]);
        close(liveness_pipe[1]);
        return EXIT_FAILURE_GUARDIAN;
    }
    if (worker == 0) {
        close(liveness_pipe[1]);
        _exit(run_worker(liveness_pipe[0], ack_path, ready_path, arguments));
    }

    close(liveness_pipe[0]);
    worker_pid = worker;
    liveness_fd = liveness_pipe[1];
    close(STDIN_FILENO);
    close(STDOUT_FILENO);
    close(STDERR_FILENO);
    int status = 0;
    while (waitpid(worker, &status, 0) < 0) {
        if (errno != EINTR) {
            return EXIT_FAILURE_GUARDIAN;
        }
    }
    worker_pid = -1;
    close(liveness_pipe[1]);
    liveness_fd = -1;
    return WIFEXITED(status) ? WEXITSTATUS(status) : EXIT_FAILURE_GUARDIAN;
}
