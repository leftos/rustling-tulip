//! The hang guard: kills a run's children when it passes its timeout.

use std::process::{Child, ExitStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub type Shared = Arc<Mutex<Child>>;

pub fn share(child: Child) -> Shared {
    Arc::new(Mutex::new(child))
}

pub struct HangGuard {
    done: Option<mpsc::Sender<()>>,
    hung: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl HangGuard {
    /// Kill every one of `children` once `timeout` passes before
    /// [`HangGuard::finish`].
    pub fn start(children: Vec<Shared>, timeout: Duration) -> Self {
        let (done, rx) = mpsc::channel::<()>();
        let hung = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&hung);
        let thread = thread::spawn(move || {
            if matches!(rx.recv_timeout(timeout), Err(RecvTimeoutError::Timeout)) {
                flag.store(true, Ordering::SeqCst);
                for child in &children {
                    if let Ok(mut child) = child.lock() {
                        let _ = child.kill();
                    }
                }
            }
        });
        Self {
            done: Some(done),
            hung,
            thread: Some(thread),
        }
    }

    /// Whether the guard fired.
    pub fn hung(&self) -> bool {
        self.hung.load(Ordering::SeqCst)
    }

    /// Stop the guard; whether it fired.
    pub fn finish(mut self) -> bool {
        drop(self.done.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.hung()
    }
}

/// Wait for `child` without holding its lock, so the guard can kill it.
pub fn wait(child: &Shared) -> std::io::Result<ExitStatus> {
    loop {
        {
            let mut locked = child
                .lock()
                .map_err(|_| std::io::Error::other("a child's lock was poisoned"))?;
            if let Some(status) = locked.try_wait()? {
                return Ok(status);
            }
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// The child's CPU time, read through its handle.
pub fn cpu_ms(child: &Shared) -> Option<u64> {
    child.lock().ok().and_then(|c| crate::cpu::child_cpu_ms(&c))
}
