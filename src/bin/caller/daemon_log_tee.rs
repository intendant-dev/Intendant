//! Tee the controller's stderr and stdout to a session-scoped
//! `daemon.log` file while still mirroring output to the original
//! terminal.
//!
//! Used by the "Download session report" button in Settings → Debug:
//! the generated zip contains `daemon.log` so controller output
//! (eprintln!, panics, tracing) travels with the rest of the session
//! artifacts when a tester sends a bundle back to the dev.
//!
//! Callers must only invoke [`install`] once per process.

#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::os::fd::{IntoRawFd, RawFd};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::{Mutex, Once};
#[cfg(unix)]
use std::thread;

/// Read ends of the tee pipes, for the exit-time drain.
#[cfg(unix)]
static TEE_READ_FDS: Mutex<Vec<RawFd>> = Mutex::new(Vec::new());
/// Bytes read from the pipes / bytes fully written to both sinks. Equal
/// counters + empty pipes = every byte that reached the teed fds is on
/// disk and on the original stream.
#[cfg(unix)]
static TEE_BYTES_IN: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
static TEE_BYTES_OUT: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
static TEE_DRAIN_HOOK: Once = Once::new();

#[cfg(unix)]
pub fn install(path: &Path) -> io::Result<()> {
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let file_fd_stderr = file.into_raw_fd();
    // SAFETY: `file_fd_stderr` is a freshly opened, owned fd; dup only
    // duplicates it and reports failure via the return value.
    let file_fd_stdout = unsafe { libc::dup(file_fd_stderr) };
    if file_fd_stdout < 0 {
        return Err(io::Error::last_os_error());
    }

    tee_fd(libc::STDERR_FILENO, file_fd_stderr)?;
    tee_fd(libc::STDOUT_FILENO, file_fd_stdout)?;

    // The copier is a background thread, so output written immediately
    // before exit — the Rust runtime printing main()'s Err to stderr is
    // the canonical case — would otherwise still be sitting in the pipe
    // when the process dies, vanishing from BOTH sinks. Drain at exit:
    // atexit handlers run after that final print, so waiting (bounded)
    // for the pipes to empty and the copier to finish writing guarantees
    // the last lines land.
    TEE_DRAIN_HOOK.call_once(|| {
        // SAFETY: registering an `extern "C"` handler with no
        // preconditions; `drain_tees_at_exit` does not unwind.
        unsafe {
            libc::atexit(drain_tees_at_exit);
        }
    });
    Ok(())
}

#[cfg(unix)]
extern "C" fn drain_tees_at_exit() {
    let Ok(fds) = TEE_READ_FDS.lock() else {
        return;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    loop {
        let pipes_empty = fds.iter().all(|&fd| {
            let mut pending: libc::c_int = 0;
            // SAFETY: fd is a live pipe read end owned by this module for
            // the life of the process; FIONREAD writes the byte count
            // into `pending` and cannot fault with a valid pointer.
            let rc = unsafe { libc::ioctl(fd, libc::FIONREAD, &mut pending) };
            rc != 0 || pending == 0
        });
        let copier_caught_up =
            TEE_BYTES_IN.load(Ordering::Acquire) == TEE_BYTES_OUT.load(Ordering::Acquire);
        if (pipes_empty && copier_caught_up) || std::time::Instant::now() >= deadline {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[cfg(not(unix))]
pub fn install(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn tee_fd(target_fd: RawFd, file_fd: RawFd) -> io::Result<()> {
    // Preserve a handle to the original terminal so the background
    // thread can still mirror output there.
    let orig_fd = unsafe { libc::dup(target_fd) };
    if orig_fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut pipe_fds = [0i32; 2];
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } < 0 {
        let err = io::Error::last_os_error();
        unsafe {
            libc::close(orig_fd);
        }
        return Err(err);
    }
    let pipe_read = pipe_fds[0];
    let pipe_write = pipe_fds[1];

    if unsafe { libc::dup2(pipe_write, target_fd) } < 0 {
        let err = io::Error::last_os_error();
        unsafe {
            libc::close(pipe_read);
            libc::close(pipe_write);
            libc::close(orig_fd);
        }
        return Err(err);
    }
    unsafe {
        libc::close(pipe_write);
    }
    if let Ok(mut fds) = TEE_READ_FDS.lock() {
        fds.push(pipe_read);
    }

    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        // Per-tee accumulator for the in-progress (newline-less) tail of
        // the file-side output. We pass bytes through to the original
        // terminal as-is (preserving ANSI codes and partial-line writes
        // that interactive output relies on), but for the daemon.log file
        // we line-buffer and prepend a wallclock timestamp to each line so
        // tester-submitted bundles are temporally analyzable later.
        let mut line_buf: Vec<u8> = Vec::with_capacity(1024);
        loop {
            let n = unsafe { libc::read(pipe_read, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n <= 0 {
                break;
            }
            let len = n as usize;
            let chunk = &buf[..len];
            TEE_BYTES_IN.fetch_add(len as u64, Ordering::AcqRel);

            // Pass-through to original terminal — unchanged, no buffering.
            unsafe {
                let _ = libc::write(orig_fd, chunk.as_ptr() as *const _, len);
            }

            // Line-buffer for the file with a per-line timestamp prefix.
            for &b in chunk {
                line_buf.push(b);
                if b == b'\n' {
                    write_timestamped_line(file_fd, &line_buf);
                    line_buf.clear();
                }
            }
            TEE_BYTES_OUT.fetch_add(len as u64, Ordering::AcqRel);
        }
        // Flush any final partial line that lacked a trailing newline.
        if !line_buf.is_empty() {
            line_buf.push(b'\n');
            write_timestamped_line(file_fd, &line_buf);
        }
        unsafe {
            libc::close(pipe_read);
        }
    });

    Ok(())
}

/// Atomically write `[timestamp] line` to `file_fd`.
///
/// The timestamp + line are concatenated into a single buffer and written
/// in one `write(2)` call. Linux guarantees write atomicity for buffers up
/// to PIPE_BUF (4096) on pipes; for regular files atomicity is not
/// formally guaranteed but Linux's filesystem layer in practice does not
/// interleave sub-call writes from different threads. Lines longer than
/// the buffer fall back to a single best-effort write.
#[cfg(unix)]
fn write_timestamped_line(file_fd: RawFd, line: &[u8]) {
    let ts = chrono::Local::now().format("%H:%M:%S%.3f ").to_string();
    let mut out = Vec::with_capacity(ts.len() + line.len());
    out.extend_from_slice(ts.as_bytes());
    out.extend_from_slice(line);
    unsafe {
        let _ = libc::write(file_fd, out.as_ptr() as *const _, out.len());
    }
}
