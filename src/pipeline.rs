use std::fs::File;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::sync::SyncError;

const STDERR_CAPTURE_LIMIT: usize = 2048;

pub fn run_command_to_temp(
    source: &Path,
    temp: &Path,
    cmd: &str,
    timeout: Duration,
) -> Result<(), SyncError> {
    let stdin = File::open(source).map_err(SyncError::Io)?;
    let stdout = File::create(temp).map_err(SyncError::Io)?;

    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::piped());

    // Put the child (and any descendants it forks) into its own process group
    // so SIGKILL on timeout reaches the whole tree, not just the shell.
    // Doing this in pre_exec guarantees the pgid is set before exec; the
    // parent-side setpgid below covers the symmetric race window.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = command.spawn().map_err(SyncError::Io)?;
    let pid = child.id() as libc::pid_t;
    // Idempotent with the child's pre_exec — fails harmlessly with EACCES
    // if the child already exec'd. Either path leaves pgid == pid.
    unsafe { libc::setpgid(pid, pid) };

    let stderr_handle = child.stderr.take().expect("stderr was configured as piped");
    let stderr_thread = std::thread::spawn(move || {
        // Capture up to LIMIT bytes, then drain the rest so the child can't
        // block writing into a full pipe. Bounding at read time keeps memory
        // bounded even if the child emits megabytes of stderr.
        let mut handle = stderr_handle;
        let mut buf = Vec::with_capacity(STDERR_CAPTURE_LIMIT);
        let _ = (&mut handle)
            .take(STDERR_CAPTURE_LIMIT as u64)
            .read_to_end(&mut buf);
        let _ = std::io::copy(&mut handle, &mut std::io::sink());
        String::from_utf8_lossy(&buf).into_owned()
    });

    // Wait for the child in a dedicated thread so the main thread can block
    // on a channel with a timeout instead of polling try_wait. On timeout we
    // SIGKILL the process group by pgid (= pid); the waiter then observes
    // the exit, sends the status, and exits.
    let (tx, rx) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        let _ = tx.send(child.wait());
    });

    let wait_result: Result<std::process::ExitStatus, std::io::Error> =
        match rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Negative pid -> kill the whole process group, taking out
                // any subprocesses the shell forked.
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = rx.recv();
                let _ = waiter.join();
                let _ = stderr_thread.join();
                return Err(SyncError::Timeout {
                    cmd: cmd.to_string(),
                    after: timeout,
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = waiter.join();
                let _ = stderr_thread.join();
                return Err(SyncError::Io(std::io::Error::other(
                    "child waiter thread disconnected before sending status",
                )));
            }
        };
    let _ = waiter.join();

    let status = match wait_result {
        Ok(s) => s,
        Err(e) => {
            let _ = stderr_thread.join();
            return Err(SyncError::Io(e));
        }
    };
    let stderr = match stderr_thread.join() {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!("stderr capture thread panicked for command '{cmd}'");
            String::new()
        }
    };

    if !status.success() {
        return Err(SyncError::Command {
            cmd: cmd.to_string(),
            exit: status.code(),
            stderr,
        });
    }

    if !stderr.is_empty() {
        tracing::warn!("Command '{cmd}' wrote to stderr: {stderr}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    #[test]
    fn test_run_command_gzip_roundtrip() {
        let dir = TempDir::new().unwrap();
        let plain = dir.path().join("plain.txt");
        let compressed = dir.path().join("compressed.gz");
        let restored = dir.path().join("restored.txt");

        fs::write(&plain, "the quick brown fox").unwrap();

        run_command_to_temp(&plain, &compressed, "gzip -c", TEST_TIMEOUT).unwrap();
        run_command_to_temp(&compressed, &restored, "gunzip -c", TEST_TIMEOUT).unwrap();

        assert_eq!(
            fs::read_to_string(&restored).unwrap(),
            "the quick brown fox"
        );
    }

    #[test]
    fn test_run_command_nonzero_exit_returns_error() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("in");
        let temp = dir.path().join("out");
        fs::write(&src, "hello").unwrap();

        let err = run_command_to_temp(&src, &temp, "exit 7", TEST_TIMEOUT).unwrap_err();
        match err {
            SyncError::Command { exit, .. } => assert_eq!(exit, Some(7)),
            other => panic!("expected SyncError::Command, got {other:?}"),
        }
    }

    #[test]
    fn test_run_command_stderr_captured_in_error() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("in");
        let temp = dir.path().join("out");
        fs::write(&src, "hello").unwrap();

        let err =
            run_command_to_temp(&src, &temp, "echo oh-no 1>&2; exit 1", TEST_TIMEOUT).unwrap_err();
        match err {
            SyncError::Command { stderr, .. } => assert!(stderr.contains("oh-no")),
            other => panic!("expected SyncError::Command, got {other:?}"),
        }
    }

    #[test]
    fn test_run_command_unknown_binary_returns_error() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("in");
        let temp = dir.path().join("out");
        fs::write(&src, "hi").unwrap();

        let err = run_command_to_temp(
            &src,
            &temp,
            "definitely-not-a-real-binary-zzz-12345",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(matches!(err, SyncError::Command { .. }));
    }

    // A pipeline command that hangs must not freeze the sync loop forever;
    // bsync runs all pairs on a single thread and one stuck command would
    // block every other pair. The runner must enforce a timeout.
    #[test]
    fn test_run_command_kills_on_timeout() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("in");
        let temp = dir.path().join("out");
        fs::write(&src, "hi").unwrap();

        let start = std::time::Instant::now();
        let err =
            run_command_to_temp(&src, &temp, "sleep 60", Duration::from_millis(200)).unwrap_err();
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "should kill quickly, took {elapsed:?}"
        );
        assert!(
            matches!(err, SyncError::Timeout { .. }),
            "expected Timeout, got {err:?}"
        );
    }

    // Without process-group SIGKILL, killing the shell would leave a forked
    // grandchild orphaned to init still consuming resources. Running the
    // shell in its own pgrp and signalling the group ensures descendants die
    // too.
    #[test]
    fn test_run_command_kills_grandchildren_on_timeout() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("in");
        let temp = dir.path().join("out");
        let pid_file = dir.path().join("grandchild.pid");
        fs::write(&src, "hi").unwrap();

        // Shell forks `sleep 60` in the background, writes its pid, then
        // waits — so the shell is still alive when the timeout fires.
        let cmd = format!("sleep 60 & echo $! > {}; wait", pid_file.display());

        let _ = run_command_to_temp(&src, &temp, &cmd, Duration::from_millis(500));

        let start = std::time::Instant::now();
        while !pid_file.exists() && start.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(pid_file.exists(), "grandchild never wrote its pid");

        let grandchild_pid: i32 = fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        // Poll for grandchild death — init may take a tick to reap it.
        // kill(pid, 0) probes existence: 0 == alive, ESRCH == dead.
        let start = std::time::Instant::now();
        let mut alive = true;
        while alive && start.elapsed() < Duration::from_secs(5) {
            let rc = unsafe { libc::kill(grandchild_pid, 0) };
            if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                alive = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        assert!(!alive, "grandchild pid {grandchild_pid} survived timeout");
    }
}
