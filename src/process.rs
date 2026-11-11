//! Bounded subprocesses shared by the owner and device helpers.
use std::cell::RefCell;
use std::io::Read;
use std::os::{
    fd::AsRawFd,
    unix::process::{CommandExt, ExitStatusExt},
};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

thread_local! {
    static CANCEL: RefCell<Option<(Arc<AtomicU64>, u64)>> = const { RefCell::new(None) };
}

pub fn with_cancel<T>(generation: Arc<AtomicU64>, expected: u64, f: impl FnOnce() -> T) -> T {
    CANCEL.with(|slot| {
        let old = slot.replace(Some((generation, expected)));
        struct Reset<'a>(
            &'a RefCell<Option<(Arc<AtomicU64>, u64)>>,
            Option<(Arc<AtomicU64>, u64)>,
        );
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.replace(self.1.take());
            }
        }
        let _reset = Reset(slot, old);
        f()
    })
}

pub fn cancelled() -> bool {
    CANCEL.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|(g, n)| g.load(Ordering::SeqCst) != *n)
    })
}

/// Each owned child has its own process group and dies if its spawning owner dies.
pub fn prepare(command: &mut Command) {
    let parent = std::process::id();
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 || libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                libc::_exit(125);
            }
            Ok(())
        });
    }
}

/// Observe exit without reaping: reserve the process/group ID until cleanup finishes.
pub fn exited(child: &Child) -> std::io::Result<Option<ExitStatus>> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id(),
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    let code = unsafe { info.si_status() };
    Ok(Some(ExitStatus::from_raw(
        if info.si_code == libc::CLD_EXITED {
            code << 8
        } else {
            code
        },
    )))
}

pub fn terminate(child: &mut Child) {
    // All callers retain an unreaped child created by prepare().
    if exited(child).is_err() {
        return;
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        if exited(child).ok().flatten().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
}

pub fn run_out(cmd: &str, args: &[&str]) -> Result<String, String> {
    run_out_timeout(cmd, args, Duration::from_secs(5))
}

pub fn run_out_timeout(cmd: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    capture(cmd, args, timeout, false)
}

// scrcpy's camera inventory is intentionally printed on stderr on some versions.
pub fn run_combined(cmd: &str, args: &[&str]) -> Result<String, String> {
    capture(cmd, args, Duration::from_secs(5), true)
}

fn capture(cmd: &str, args: &[&str], timeout: Duration, combined: bool) -> Result<String, String> {
    if cancelled() {
        return Err("Operation cancelled".into());
    }
    let mut command = Command::new(cmd);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    prepare(&mut command);
    let mut child = command.spawn().map_err(|e| format!("{cmd}: {e}"))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let result = unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) };
        if result == -1 {
            terminate(&mut child);
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>) -> Result<(), String> {
        let mut buffer = [0; 8192];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    if bytes.len() + n > 262_144 {
                        return Err("output limit exceeded".into());
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    let mut out = Vec::new();
    let mut err = Vec::new();
    let deadline = Instant::now() + timeout;
    let result = loop {
        if cancelled() {
            break Err("Operation cancelled".into());
        }
        if Instant::now() >= deadline {
            break Err(format!("{cmd}: timed out"));
        }
        if let Err(e) = drain(&mut stdout, &mut out).and_then(|()| drain(&mut stderr, &mut err)) {
            break Err(format!("{cmd}: {e}"));
        }
        match exited(&child) {
            Ok(Some(status)) => {
                let read = drain(&mut stdout, &mut out).and_then(|()| drain(&mut stderr, &mut err));
                break read.map(|()| status);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => break Err(format!("{cmd}: {e}")),
        }
    };
    terminate(&mut child);
    let status = result?;
    let stdout = String::from_utf8_lossy(&out).into_owned();
    let stderr = String::from_utf8_lossy(&err);
    if status.success() {
        Ok(if combined {
            format!("{stdout}\n{stderr}")
        } else {
            stdout
        })
    } else {
        Err(format!(
            "{cmd}: {}",
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn commands_are_bounded_and_cancellable() {
        let start = Instant::now();
        assert!(
            run_out_timeout("sh", &["-c", "sleep 30"], Duration::from_millis(60))
                .unwrap_err()
                .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(1));
        let generation = Arc::new(AtomicU64::new(1));
        assert!(with_cancel(generation, 0, || run_out("true", &[]))
            .unwrap_err()
            .contains("cancelled"));
        assert!(run_out("sh", &["-c", "head -c 300000 /dev/zero"])
            .unwrap_err()
            .contains("output limit"));
        assert_eq!(
            run_out("sh", &["-c", "printf stdout; printf diagnostic >&2"]).unwrap(),
            "stdout"
        );
        assert!(run_combined("sh", &["-c", "printf diagnostic >&2"])
            .unwrap()
            .contains("diagnostic"));
    }
    #[test]
    fn escaped_pipe_holder_cannot_hang_capture() {
        let start = Instant::now();
        // An inherited open pipe must never cause a blocking reader join.
        assert!(run_out("sh", &["-c", "sleep 0.5 & printf done"])
            .unwrap()
            .contains("done"));
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
