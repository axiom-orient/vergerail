#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::fs::OpenOptions;
use std::io;
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::process::{Child, Command};

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const GUARDIAN_BYTES: &[u8] = include_bytes!(env!("VERGERAIL_GUARDIAN_PATH"));
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const GUARDIAN_SHA256: &str = env!("VERGERAIL_GUARDIAN_SHA256");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
static GUARDIAN_SEQUENCE: AtomicU64 = AtomicU64::new(1);
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
static GUARDIAN_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(1);

/// The only process identity retained by Rust is its directly owned guardian.
/// The guardian owns the Codex leader and keeps it unreaped until its private
/// process group has been scanned and torn down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessIdentity {
    pub(crate) process_id: Option<u32>,
}

impl ProcessIdentity {
    #[cfg(test)]
    pub(crate) const fn none() -> Self {
        Self { process_id: None }
    }
}

/// Materializes the audited build-packaged guardian in an owner-only directory.
/// The bytes are embedded in the consumer binary, so relocating that binary
/// does not change the helper lookup path or trust boundary.
pub(crate) fn extract_guardian(directory: &Path) -> io::Result<PathBuf> {
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let _ = directory;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the guardian runtime is supported only on aarch64 macOS",
        ))
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        verify_private_directory(directory)?;
        let sequence = GUARDIAN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            ".vergerail-guardian-{}-{sequence}",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        file.write_all(GUARDIAN_BYTES)?;
        file.sync_all()?;
        set_private_executable_permissions(&file, &path)?;
        if hash_bytes(GUARDIAN_BYTES) != GUARDIAN_SHA256 {
            let _ = fs::remove_file(&path);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "embedded guardian bytes do not match the build-time digest",
            ));
        }
        Ok(path)
    }
}

/// Creates a fresh private directory for a short-lived guardian extraction.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn create_guardian_directory(parent: &Path, prefix: &str) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;

    let sequence = GUARDIAN_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = parent.join(format!(".{prefix}-{}-{sequence}", std::process::id()));
    fs::create_dir(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// Removes a private extraction directory after its helper has been removed.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) fn remove_guardian_directory(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Constructs the guardian command. Its child inherits all stdio, environment,
/// and current-directory settings from the caller; the guardian does not proxy
/// JSONL or rewrite any transport bytes.
pub(crate) fn command(path: &Path, entrypoint: &Path) -> Command {
    let mut command = Command::new(path);
    command.arg("--").arg(entrypoint);
    command
}

/// Captures the direct guardian identity before it can be awaited or dropped.
pub(crate) fn capture(child: &Child) -> io::Result<ProcessIdentity> {
    let process_id = child.id().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "guardian exited before its owned process identity was captured",
        )
    })?;
    Ok(ProcessIdentity {
        process_id: Some(process_id),
    })
}

/// Sends TERM only to the still-owned, unreaped guardian child. A missing or
/// changed Child identity is a custody failure; there is no numeric PGID
/// fallback after the direct child is reaped.
pub(crate) fn terminate(identity: ProcessIdentity, child: &mut Child) -> io::Result<()> {
    let captured = identity.process_id.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "no owned guardian identity is available",
        )
    })?;
    let current = child.id().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "guardian has already been reaped; no signal was sent",
        )
    })?;
    if current != captured {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "owned guardian identity changed before termination",
        ));
    }

    #[cfg(unix)]
    {
        use rustix::io::Errno;
        use rustix::process::{Pid, Signal, kill_process};

        let pid = Pid::from_raw(captured as _).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned guardian process id is invalid",
            )
        })?;
        match kill_process(pid, Signal::TERM) {
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            Err(error) => Err(io::Error::from_raw_os_error(error.raw_os_error())),
        }
    }
    #[cfg(not(unix))]
    {
        child.start_kill()
    }
}

/// Removes a single per-run helper after the guardian has been reaped.
pub(crate) fn remove_guardian(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn verify_private_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guardian directory must be a non-symlink owner-only directory",
        ));
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn set_private_executable_permissions(file: &std::fs::File, path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    file.set_permissions(fs::Permissions::from_mode(0o700))?;
    let metadata = fs::symlink_metadata(path)?;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "guardian helper is not owner-only",
        ));
    }
    Ok(())
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use super::*;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use std::fs;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use std::panic::AssertUnwindSafe;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use std::process::{Child as HostChild, ExitStatus};
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use std::process::{Command as HostCommand, Stdio};
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use std::sync::{Arc, Mutex};
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use std::time::{Duration, Instant};
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    use tokio::time::timeout;

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    async fn wait_for_file(path: &Path, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if path.exists() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    async fn wait_for_pid_state(pid: u32, present: bool, limit: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + limit;
        loop {
            let pid_text = pid.to_string();
            let output = HostCommand::new("/bin/ps")
                .args(["-p", &pid_text, "-o", "pid="])
                .output()?;
            let is_present = !output.stdout.is_empty();
            if is_present == present {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    async fn assert_pid_absent_twice(pid: u32) -> io::Result<()> {
        if !wait_for_pid_state(pid, false, Duration::from_secs(4)).await? {
            return Err(io::Error::other(format!(
                "owned fixture pid {pid} remained live"
            )));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        if !wait_for_pid_state(pid, false, Duration::from_secs(1)).await? {
            return Err(io::Error::other(format!(
                "owned fixture pid {pid} reappeared during delayed watch"
            )));
        }
        Ok(())
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn read_fixture_pid(path: &Path) -> io::Result<u32> {
        fs::read_to_string(path)?.trim().parse().map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("fixture pid is not numeric: {error}"),
            )
        })
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn write_late_fork_fixture(path: &Path) -> io::Result<()> {
        fs::write(
            path,
            r#"use strict;
use warnings;
my ($leader_started, $ready, $release, $child_started, $child_release, $marker) = @ARGV;
$SIG{ALRM} = sub { exit 125 };
alarm 10;
$SIG{TERM} = sub {
    open my $ready_file, '>', $ready or die "ready: $!";
    close $ready_file or die "ready close: $!";
    1 while !-e $release;
    my $child = fork();
    die "fork: $!" unless defined $child;
    if ($child == 0) {
        alarm 5;
        close STDIN;
        close STDOUT;
        close STDERR;
        open my $started_file, '>', $child_started or die "started: $!";
        print {$started_file} "$$";
        close $started_file or die "started close: $!";
        $SIG{TERM} = 'IGNORE';
        1 while !-e $child_release;
        open my $marker_file, '>', $marker or die "marker: $!";
        print {$marker_file} "survived";
        close $marker_file or die "marker close: $!";
        exit 0;
    }
    1 while !-e $child_started;
    exit 0;
};
open my $leader_started_file, '>', $leader_started or die "leader started: $!";
print {$leader_started_file} "$$";
close $leader_started_file or die "leader started close: $!";
sleep 10 while 1;
"#,
        )
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn compile_legacy_fixture(directory: &Path) -> io::Result<PathBuf> {
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("legacy_guardian_mutant.c");
        let output = directory.join("vergerail-test-legacy-fixture");
        let status = HostCommand::new("/usr/bin/clang")
            .args([
                "-std=c11",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-fno-common",
            ])
            .arg(source)
            .arg("-o")
            .arg(&output)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("test fixture clang compile failed"));
        }
        let status = HostCommand::new("codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(&output)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("test fixture ad-hoc signing failed"));
        }
        Ok(output)
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[derive(Debug, Default)]
    struct MutationCleanupReport {
        direct_guardian_waited: bool,
        leader_absent: bool,
        late_child_absent: bool,
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    struct MutationCleanupGuard {
        child: Option<HostChild>,
        release: PathBuf,
        child_release: PathBuf,
        fixture_marker: PathBuf,
        leader_pid: Option<u32>,
        late_child_pid: Option<u32>,
        report: Arc<Mutex<MutationCleanupReport>>,
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    impl MutationCleanupGuard {
        fn new(
            child: HostChild,
            release: PathBuf,
            child_release: PathBuf,
            fixture_marker: PathBuf,
            report: Arc<Mutex<MutationCleanupReport>>,
        ) -> Self {
            Self {
                child: Some(child),
                release,
                child_release,
                fixture_marker,
                leader_pid: None,
                late_child_pid: None,
                report,
            }
        }

        fn guardian_pid(&self) -> io::Result<u32> {
            self.child
                .as_ref()
                .map(HostChild::id)
                .ok_or_else(|| io::Error::other("mutation guardian was already reaped"))
        }

        fn set_leader_pid(&mut self, pid: u32) {
            self.leader_pid = Some(pid);
        }

        fn set_late_child_pid(&mut self, pid: u32) {
            self.late_child_pid = Some(pid);
        }

        fn wait_guardian(&mut self) -> io::Result<ExitStatus> {
            let status = {
                let child = self
                    .child
                    .as_mut()
                    .ok_or_else(|| io::Error::other("mutation guardian was already reaped"))?;
                wait_host_child(child, Duration::from_secs(4))?
            };
            match status {
                Some(status) => {
                    self.child.take();
                    if let Ok(mut report) = self.report.lock() {
                        report.direct_guardian_waited = true;
                    }
                    Ok(status)
                }
                None => Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "mutation guardian did not exit within the bounded wait",
                )),
            }
        }

        fn cleanup(&mut self) {
            // Release every fixture gate first, then terminate the directly owned
            // guardian and wait for it before touching the recorded descendants.
            let _ = fs::write(&self.release, b"cleanup");
            let _ = fs::write(&self.child_release, b"cleanup");

            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let waited = wait_host_child(child, Duration::from_secs(4))
                    .ok()
                    .flatten()
                    .is_some();
                if let Ok(mut report) = self.report.lock() {
                    report.direct_guardian_waited = waited;
                }
            }

            let recorded = [self.leader_pid, self.late_child_pid];
            for pid in recorded.into_iter().flatten() {
                kill_recorded_pid(pid, &self.fixture_marker);
            }
            if let Some(pid) = self.leader_pid {
                let absent =
                    assert_fixture_pid_absent_twice_blocking(pid, &self.fixture_marker).is_ok();
                if let Ok(mut report) = self.report.lock() {
                    report.leader_absent = absent;
                }
            }
            if let Some(pid) = self.late_child_pid {
                let absent =
                    assert_fixture_pid_absent_twice_blocking(pid, &self.fixture_marker).is_ok();
                if let Ok(mut report) = self.report.lock() {
                    report.late_child_absent = absent;
                }
            }
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    impl Drop for MutationCleanupGuard {
        fn drop(&mut self) {
            self.cleanup();
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn wait_host_child(child: &mut HostChild, limit: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn wait_for_file_blocking(path: &Path, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        loop {
            if path.exists() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn wait_for_fixture_pid_state_blocking(
        pid: u32,
        fixture_marker: &Path,
        present: bool,
        limit: Duration,
    ) -> io::Result<bool> {
        let deadline = Instant::now() + limit;
        let fixture_marker = fixture_marker.to_string_lossy();
        loop {
            let pid_text = pid.to_string();
            let output = HostCommand::new("/bin/ps")
                .args(["-p", &pid_text, "-o", "pid=,command="])
                .output()?;
            let process_text = String::from_utf8_lossy(&output.stdout);
            let is_present =
                !process_text.is_empty() && process_text.contains(fixture_marker.as_ref());
            if is_present == present {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn assert_fixture_pid_absent_twice_blocking(pid: u32, fixture_marker: &Path) -> io::Result<()> {
        if !wait_for_fixture_pid_state_blocking(pid, fixture_marker, false, Duration::from_secs(4))?
        {
            return Err(io::Error::other(format!(
                "owned mutation fixture pid {pid} remained live"
            )));
        }
        std::thread::sleep(Duration::from_millis(100));
        if !wait_for_fixture_pid_state_blocking(pid, fixture_marker, false, Duration::from_secs(1))?
        {
            return Err(io::Error::other(format!(
                "owned mutation fixture pid {pid} reappeared during delayed watch"
            )));
        }
        Ok(())
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn signal_host_pid(pid: u32, signal: &str) -> io::Result<()> {
        let pid_text = pid.to_string();
        let status = HostCommand::new("/bin/kill")
            .args([signal, &pid_text])
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "kill {signal} {pid} exited with {status}"
            )))
        }
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn kill_recorded_pid(pid: u32, fixture_marker: &Path) {
        let fixture_marker = fixture_marker.to_string_lossy();
        let pid_text = pid.to_string();
        let process = HostCommand::new("/bin/ps")
            .args(["-p", &pid_text, "-o", "pid=,command="])
            .output();
        let process_matches = process
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).contains(fixture_marker.as_ref()))
            .unwrap_or(false);
        if !process_matches {
            return;
        }
        let pid_text = pid.to_string();
        let _ = HostCommand::new("/bin/kill")
            .args(["-KILL", &pid_text])
            .stderr(Stdio::null())
            .status();
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn assert_mutation_cleanup_report(report: &Arc<Mutex<MutationCleanupReport>>, scenario: &str) {
        let report = report.lock().expect("mutation cleanup report");
        assert!(
            report.direct_guardian_waited,
            "{scenario}: direct guardian was not waited"
        );
        assert!(report.leader_absent, "{scenario}: leader remained present");
        assert!(
            report.late_child_absent,
            "{scenario}: late child remained present"
        );
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn embedded_guardian_digest_is_self_consistent() {
        assert_eq!(hash_bytes(GUARDIAN_BYTES), GUARDIAN_SHA256);
    }

    #[test]
    fn custody_source_has_no_reap_then_numeric_group_signal_path() {
        let source = include_str!("process_tree.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production process custody source");
        assert!(!production.contains("kill_group"));
        assert!(!production.contains("process_group_id"));
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn guardian_extraction_is_owner_only_and_relocatable() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().expect("guardian directory");
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("private directory");
        let helper = extract_guardian(directory.path()).expect("extract guardian");
        assert!(helper.is_absolute());
        assert_eq!(
            fs::metadata(&helper)
                .expect("helper metadata")
                .permissions()
                .mode()
                & 0o077,
            0
        );
        remove_guardian(&helper).expect("remove guardian");
        assert!(!helper.exists());
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[tokio::test]
    async fn guardian_reaps_leader_and_closes_pipe_after_descendant_exit() {
        let directory = tempfile::tempdir().expect("guardian directory");
        let marker = directory.path().join("escaped-marker");
        let helper = extract_guardian(directory.path()).expect("extract guardian");
        let script = format!(
            "(sleep 30; printf survived > '{}') & exit 0",
            marker.display()
        );
        let mut command = command(&helper, Path::new("/bin/sh"));
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = timeout(Duration::from_secs(3), command.output())
            .await
            .expect("guardian must not leave an open pipe")
            .expect("guardian command");
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!marker.exists(), "same-pgrp descendant survived teardown");
        remove_guardian(&helper).expect("remove helper");
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[tokio::test]
    async fn guardian_liveness_pipe_tears_down_when_owned_child_is_killed() {
        let directory = tempfile::tempdir().expect("guardian directory");
        let marker = directory.path().join("liveness-marker");
        let helper = extract_guardian(directory.path()).expect("extract guardian");
        let script = format!(
            "(sleep 30; printf survived > '{}') & sleep 30",
            marker.display()
        );
        let mut command = command(&helper, Path::new("/bin/sh"));
        command
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("guardian command");
        let _ = child.start_kill();
        let status = timeout(Duration::from_secs(3), child.wait())
            .await
            .expect("owned guardian reap")
            .expect("guardian wait");
        assert!(!status.success());
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(
            !marker.exists(),
            "liveness teardown left a same-session child"
        );
        remove_guardian(&helper).expect("remove helper");
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[tokio::test]
    async fn guardian_term_ignoring_leader_is_force_killed_and_reaped() {
        let directory = tempfile::tempdir().expect("guardian directory");
        let leader_started = directory.path().join("term-leader-started");
        let ready = directory.path().join("term-ready");
        let release = directory.path().join("term-release");
        let child_started = directory.path().join("late-child-started");
        let child_release = directory.path().join("late-child-release");
        let marker = directory.path().join("term-ignored-marker");
        let fixture = directory.path().join("term-late-fork.pl");
        let helper = extract_guardian(directory.path()).expect("extract guardian");
        write_late_fork_fixture(&fixture).expect("write deterministic TERM fixture");
        let mut command = command(&helper, Path::new("/usr/bin/perl"));
        command
            .arg(&fixture)
            .arg(&leader_started)
            .arg(&ready)
            .arg(&release)
            .arg(&child_started)
            .arg(&child_release)
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("guardian command");
        let identity = capture(&child).expect("guardian identity");
        assert!(
            wait_for_file(&leader_started, Duration::from_secs(2)).await,
            "leader fixture did not start"
        );
        let leader_pid = read_fixture_pid(&leader_started).expect("leader fixture pid");
        terminate(identity, &mut child).expect("terminate owned guardian");
        assert!(
            wait_for_file(&ready, Duration::from_secs(2)).await,
            "leader TERM handler did not reach its pre-fork gate"
        );
        fs::write(&release, b"release").expect("release late fork");
        assert!(
            wait_for_file(&child_started, Duration::from_secs(2)).await,
            "TERM handler did not fork the late child"
        );
        let child_pid = read_fixture_pid(&child_started).expect("late child fixture pid");
        let output = timeout(Duration::from_secs(4), child.wait_with_output())
            .await
            .expect("TERM-ignoring leader must be force-killed in bounded time")
            .expect("guardian wait");
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        fs::write(&child_release, b"release").expect("release survivor probe");
        assert!(
            !wait_for_file(&marker, Duration::from_millis(500)).await,
            "late child survived the guardian cleanup"
        );
        assert!(
            !marker.exists(),
            "TERM-ignoring same-pgrp survivor remained"
        );
        assert_pid_absent_twice(leader_pid)
            .await
            .expect("leader process was reaped");
        assert_pid_absent_twice(child_pid)
            .await
            .expect("late child was killed and reaped");
        remove_guardian(&helper).expect("remove helper");
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[derive(Clone, Copy)]
    enum MutationScenario {
        Normal,
        Error,
        Panic,
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    fn run_legacy_mutation_scenario(
        scenario: MutationScenario,
        report: Arc<Mutex<MutationCleanupReport>>,
    ) -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let leader_started = directory.path().join("mutant-leader-started");
        let ready = directory.path().join("mutant-ready");
        let release = directory.path().join("mutant-release");
        let child_started = directory.path().join("mutant-child-started");
        let child_release = directory.path().join("mutant-child-release");
        let marker = directory.path().join("mutant-survived");
        let ack = directory.path().join("mutant-first-empty-scan");
        let fixture = directory.path().join("mutant-late-fork.pl");
        write_late_fork_fixture(&fixture)?;
        let mutant = compile_legacy_fixture(directory.path())?;

        let mut command = HostCommand::new(&mutant);
        command
            .arg("--legacy-ack")
            .arg(&ack)
            .arg("--wait-for")
            .arg(&ready)
            .arg("--")
            .arg("/usr/bin/perl")
            .arg(&fixture)
            .arg(&leader_started)
            .arg(&ready)
            .arg(&release)
            .arg(&child_started)
            .arg(&child_release)
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        let mut guard = MutationCleanupGuard::new(
            child,
            release.clone(),
            child_release.clone(),
            fixture.clone(),
            report,
        );
        let mutation_result = (|| -> io::Result<()> {
            if !wait_for_file_blocking(&leader_started, Duration::from_secs(2)) {
                return Err(io::Error::other("mutation fixture did not start"));
            }
            guard.set_leader_pid(read_fixture_pid(&leader_started)?);
            signal_host_pid(guard.guardian_pid()?, "-TERM")?;
            if !wait_for_file_blocking(&ready, Duration::from_secs(2)) {
                return Err(io::Error::other("mutation TERM handler did not reach gate"));
            }
            if !wait_for_file_blocking(&ack, Duration::from_secs(2)) {
                return Err(io::Error::other(
                    "test-only first-empty-scan fixture did not acknowledge",
                ));
            }
            if fs::read_to_string(&ack)?.trim() != "first-empty-scan" {
                return Err(io::Error::other(
                    "test-only scan acknowledgement was malformed",
                ));
            }
            fs::write(&release, b"release")?;
            if !wait_for_file_blocking(&child_started, Duration::from_secs(2)) {
                return Err(io::Error::other("mutation fixture did not fork late child"));
            }
            let late_child_pid = read_fixture_pid(&child_started)?;
            guard.set_late_child_pid(late_child_pid);
            if !wait_for_fixture_pid_state_blocking(
                late_child_pid,
                &fixture,
                true,
                Duration::from_secs(1),
            )? {
                return Err(io::Error::other("late mutation child was not observable"));
            }

            match scenario {
                MutationScenario::Normal => {
                    let status = guard.wait_guardian()?;
                    if !status.success() {
                        return Err(io::Error::other(format!(
                            "mutation fixture exited with {status}"
                        )));
                    }
                    fs::write(&child_release, b"release")?;
                    if !wait_for_file_blocking(&marker, Duration::from_secs(1)) {
                        return Err(io::Error::other(
                            "test-only mutation did not expose the expected late survivor",
                        ));
                    }
                    Ok(())
                }
                MutationScenario::Error => Err(io::Error::other(
                    "test-only injected mutation error after late child start",
                )),
                MutationScenario::Panic => {
                    panic!("test-only injected mutation panic after late child start")
                }
            }
        })();
        drop(guard);
        mutation_result
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[test]
    fn legacy_mutant_fixture_proves_old_survivor_and_unconditional_cleanup() -> io::Result<()> {
        let normal_report = Arc::new(Mutex::new(MutationCleanupReport::default()));
        run_legacy_mutation_scenario(MutationScenario::Normal, Arc::clone(&normal_report))?;
        assert_mutation_cleanup_report(&normal_report, "normal");

        let error_report = Arc::new(Mutex::new(MutationCleanupReport::default()));
        assert!(
            run_legacy_mutation_scenario(MutationScenario::Error, Arc::clone(&error_report))
                .is_err()
        );
        assert_mutation_cleanup_report(&error_report, "error");

        let panic_report = Arc::new(Mutex::new(MutationCleanupReport::default()));
        let panic_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            run_legacy_mutation_scenario(MutationScenario::Panic, Arc::clone(&panic_report))
        }));
        assert!(panic_result.is_err(), "panic injection did not panic");
        assert_mutation_cleanup_report(&panic_report, "panic");
        Ok(())
    }

    #[test]
    fn guardian_cleanup_source_orders_production_scan_before_reap() {
        let source = include_str!("../native/vergerail_guardian.c");
        let cleanup = source
            .split("static int terminate_private_group")
            .nth(1)
            .expect("guardian cleanup implementation")
            .split("static int reap_leader")
            .next()
            .expect("guardian cleanup body");
        assert!(cleanup.contains("wait_for_leader_exit(leader, status, 0)"));
        assert!(cleanup.contains("signal_private_group(leader, SIGKILL, 1)"));
        let first_scan = cleanup
            .find("group_has_no_other_member")
            .expect("immediate group scan");
        let delayed_sleep = cleanup[first_scan..]
            .find("sleep_milliseconds(SCAN_DELAY_MS)")
            .map(|offset| first_scan + offset)
            .expect("delayed scan grace");
        let second_scan = cleanup[delayed_sleep..]
            .find("group_has_no_other_member")
            .map(|offset| delayed_sleep + offset)
            .expect("delayed group scan");
        let success = cleanup.find("return 0;").expect("cleanup success path");
        assert!(first_scan < delayed_sleep);
        assert!(delayed_sleep < second_scan);
        assert!(second_scan < success);

        let observation = source
            .split("static int observe_leader")
            .nth(1)
            .expect("leader observation implementation")
            .split("static int signal_private_group")
            .next()
            .expect("leader observation body");
        assert!(observation.contains("WNOWAIT"));

        let worker = source
            .split("static int run_worker")
            .nth(1)
            .expect("worker implementation");
        let cleanup_call = worker
            .find("terminate_private_group(leader, &status)")
            .expect("worker cleanup call");
        let reap_call = worker[cleanup_call..]
            .find("reap_leader(leader, &status)")
            .map(|offset| cleanup_call + offset)
            .expect("caller reap call");
        assert!(cleanup_call < reap_call);
    }

    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    #[tokio::test]
    async fn guardian_startup_exec_failure_cleanup_is_typed_and_bounded() {
        let directory = tempfile::tempdir().expect("guardian directory");
        let helper = extract_guardian(directory.path()).expect("extract guardian");
        let mut command = command(&helper, Path::new("/vergerail/missing-entrypoint"));
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = timeout(Duration::from_secs(3), command.output())
            .await
            .expect("startup failure must be bounded")
            .expect("guardian command");
        assert_eq!(output.status.code(), Some(70));
        remove_guardian(&helper).expect("remove helper");
    }
}
