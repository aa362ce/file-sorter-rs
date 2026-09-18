use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use is_terminal::IsTerminal;

/// Reports periodic progress to stderr without cluttering stdout.
///
/// On a TTY, updates a single line in place. Otherwise (piped/redirected
/// output), emits one line per interval so the log stays readable. Safe to
/// share across threads (e.g. rayon workers) via `&Progress` -- `update` only
/// needs a shared reference.
pub struct Progress {
    label: String,
    total: Option<u64>,
    interval: Duration,
    enabled: bool,
    is_tty: bool,
    count: AtomicU64,
    last_emit: Mutex<Instant>,
}

impl Progress {
    pub fn new(label: impl Into<String>, total: Option<u64>, enabled: bool) -> Self {
        let is_tty = enabled && std::io::stderr().is_terminal();
        Progress {
            label: label.into(),
            total,
            interval: Duration::from_millis(200),
            enabled,
            is_tty,
            count: AtomicU64::new(0),
            // Far enough in the past that the very first update always emits.
            last_emit: Mutex::new(Instant::now() - Duration::from_secs(3600)),
        }
    }

    pub fn update(&self, n: u64) {
        let new_count = self.count.fetch_add(n, Ordering::Relaxed) + n;
        if !self.enabled {
            return;
        }
        let now = Instant::now();
        let mut last = self.last_emit.lock().unwrap();
        if now.duration_since(*last) >= self.interval {
            *last = now;
            drop(last);
            self.emit(new_count);
        }
    }

    fn emit(&self, count: u64) {
        let msg = match self.total {
            Some(t) => format!("{}: {}/{}", self.label, count, t),
            None => format!("{}: {}", self.label, count),
        };
        let mut stderr = std::io::stderr();
        if self.is_tty {
            let _ = write!(stderr, "\r\x1b[K{}", msg);
        } else {
            let _ = writeln!(stderr, "{}", msg);
        }
        let _ = stderr.flush();
    }

    #[allow(dead_code)]
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn close(&self) {
        if self.enabled {
            self.emit(self.count.load(Ordering::Relaxed));
            if self.is_tty {
                let mut stderr = std::io::stderr();
                let _ = writeln!(stderr);
                let _ = stderr.flush();
            }
        }
    }
}
