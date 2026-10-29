use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;

static PIPE: AtomicI32 = AtomicI32::new(-1);

#[cfg(any(target_os = "linux", target_os = "android"))]
use libc::__errno_location as errno_location;
#[cfg(any(target_os = "freebsd", target_os = "dragonfly"))]
use libc::__error as errno_location;

extern "C" fn deliver(_: libc::c_int) {
    // SAFETY: errno is thread-local and restored before returning; write is async-signal-safe.
    unsafe {
        let errno = errno_location();
        let saved = *errno;
        libc::write(PIPE.load(Ordering::Relaxed), [0u8].as_ptr().cast(), 1);
        *errno = saved;
    }
}

struct Signals(File);

impl Signals {
    fn new(sigs: &[libc::c_int]) -> io::Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: fds has room for the two descriptors pipe2 writes.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe2 just returned fds[0] and nothing else owns it.
        let rx = Signals(unsafe { File::from_raw_fd(fds[0]) });
        // SAFETY: pipe2 just returned fds[1] and nothing else owns it.
        let tx = unsafe { OwnedFd::from_raw_fd(fds[1]) };
        // SAFETY: tx is an open descriptor.
        if unsafe { libc::fcntl(tx.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        PIPE.compare_exchange(-1, tx.as_raw_fd(), Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| io::Error::other("signals are already trapped"))?;
        let _ = tx.into_raw_fd();
        for &s in sigs {
            // SAFETY: an all-zero sigaction is valid, and deliver only does async-signal-safe work.
            unsafe {
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = deliver as extern "C" fn(libc::c_int) as libc::sighandler_t;
                sa.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut sa.sa_mask);
                if libc::sigaction(s, &sa, std::ptr::null_mut()) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
        }
        Ok(rx)
    }

    fn wait(&mut self) -> io::Result<()> {
        self.0.read_exact(&mut [0])
    }
}

pub fn trap(sigs: &[libc::c_int], stop: Arc<AtomicBool>, before_exit: fn()) -> io::Result<Arc<AtomicBool>> {
    let mut signals = Signals::new(sigs)?;
    let signaled = Arc::new(AtomicBool::new(false));
    let g = signaled.clone();
    thread::Builder::new().name("signals".into()).spawn(move || {
        while signals.wait().is_ok() {
            if g.swap(true, Ordering::SeqCst) {
                before_exit();
                // SAFETY: _exit ends the process at once; nothing after it relies on cleanup.
                unsafe { libc::_exit(130) };
            }
            stop.store(true, Ordering::SeqCst);
        }
    })?;
    Ok(signaled)
}
