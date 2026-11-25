//! Copies of files on their way, in either direction, as the overlay shows
//! them (docs/PRD-file-transfer.md): progress for one that takes longer
//! than a second, and Esc to cancel it (F6); the question before sending a
//! big one (F7), answered with Y or N.
//!
//! These keys work while input is local, like D: while it goes to the Mac,
//! every key is the Mac's. The badge says to release input first.
//!
//! Shared state, because the network tasks start and end transfers and the
//! window's loop draws them and reads the keys.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use auroval_proto::files::megabytes;
use tokio::sync::{Notify, oneshot};

use crate::overlay::{Badge, Hint, Tone};

/// Shown only for transfers that take longer than this.
const QUIET: Duration = Duration::from_secs(1);
/// How often the numbers in the badge change: each change draws it again.
const REDRAW: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Way {
    ToMac,
    FromMac,
}

struct Running {
    id: u64,
    way: Way,
    name: String,
    total: u64,
    done: Arc<AtomicU64>,
    since: Instant,
    cancel: Arc<Notify>,
    /// What the badge shows, and since when.
    shown: (u64, Instant),
}

struct Question {
    id: u64,
    what: String,
    answer: Option<oneshot::Sender<bool>>,
}

static NEXT: AtomicU64 = AtomicU64::new(1);
static RUNNING: Mutex<Vec<Running>> = Mutex::new(Vec::new());
static QUESTION: Mutex<Option<Question>> = Mutex::new(None);

/// A transfer the overlay shows while it lives.
pub struct Transfer {
    id: u64,
    done: Arc<AtomicU64>,
    cancel: Arc<Notify>,
}

/// Starts showing a transfer of `total` bytes of `name`.
pub fn begin(way: Way, name: &str, total: u64) -> Transfer {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let done = Arc::new(AtomicU64::new(0));
    let cancel = Arc::new(Notify::new());
    let now = Instant::now();
    RUNNING.lock().unwrap().push(Running {
        id,
        way,
        name: name.into(),
        total,
        done: done.clone(),
        since: now,
        cancel: cancel.clone(),
        shown: (0, now),
    });
    Transfer { id, done, cancel }
}

impl Transfer {
    pub fn advance(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Resolves when the user cancels it.
    pub async fn cancelled(&self) {
        self.cancel.notified().await;
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        RUNNING.lock().unwrap().retain(|r| r.id != self.id);
    }
}

/// Asks the user whether to send `what` (F7), and waits for the answer.
/// A question nobody answers stays until a newer copy replaces it.
pub async fn ask(what: String) -> bool {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    *QUESTION.lock().unwrap() = Some(Question {
        id,
        what,
        answer: Some(tx),
    });
    // Takes the question down however this ends (a newer copy aborts it).
    struct Asked(u64);
    impl Drop for Asked {
        fn drop(&mut self) {
            let mut q = QUESTION.lock().unwrap();
            if q.as_ref().is_some_and(|q| q.id == self.0) {
                *q = None;
            }
        }
    }
    let _asked = Asked(id);
    rx.await.unwrap_or(false)
}

/// Whether a question waits for Y or N.
pub fn asking() -> bool {
    QUESTION.lock().unwrap().is_some()
}

/// The user's answer to the question, if one waits.
pub fn answer(yes: bool) {
    if let Some(mut q) = QUESTION.lock().unwrap().take()
        && let Some(tx) = q.answer.take()
    {
        let _ = tx.send(yes);
    }
}

/// Whether a transfer the overlay shows runs, for Esc to cancel.
pub fn shown() -> bool {
    let now = Instant::now();
    RUNNING
        .lock()
        .unwrap()
        .iter()
        .any(|r| now - r.since >= QUIET)
}

/// Cancels every transfer that runs.
pub fn cancel() {
    for r in RUNNING.lock().unwrap().iter() {
        // Kept if nobody waits yet, so it's never lost.
        r.cancel.notify_one();
    }
}

/// The overlay's badges for transfers and the question. `grabbed`: input
/// goes to the Mac, so the keys need `host_key` first.
pub fn badges(host_key: &str, grabbed: bool) -> Vec<Badge> {
    let release = |action: &str| {
        if grabbed {
            format!("{host_key} to release input, then {action}")
        } else {
            String::new()
        }
    };
    let mut out = Vec::new();
    if let Some(q) = QUESTION.lock().unwrap().as_ref() {
        out.push(Badge {
            tag: "FILES".into(),
            tone: Tone::Amber,
            body: "Copy this to the Mac?".into(),
            detail: join(&q.what, &release("Y or N")),
            hints: vec![hint("Y", "Copy", true), hint("N", "Don't", false)],
        });
    }
    let now = Instant::now();
    for r in RUNNING.lock().unwrap().iter_mut() {
        if now - r.since < QUIET {
            continue;
        }
        if now - r.shown.1 >= REDRAW {
            r.shown = (r.done.load(Ordering::Relaxed), now);
        }
        let body = match r.way {
            Way::ToMac => format!("Copying {} to the Mac", r.name),
            Way::FromMac => format!("Copying {} from the Mac", r.name),
        };
        out.push(Badge {
            tag: "FILES".into(),
            tone: Tone::Blue,
            body,
            detail: join(&progress(r.shown.0, r.total), &release("Esc")),
            hints: vec![hint("Esc", "Cancel", false)],
        });
    }
    out
}

/// `12.0 MB of 300.0 MB (4%)`.
pub fn progress(done: u64, total: u64) -> String {
    let pct = (done.saturating_mul(100))
        .checked_div(total)
        .unwrap_or(100)
        .min(100);
    format!("{} of {} ({pct}%)", megabytes(done), megabytes(total))
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (_, true) => a.into(),
        (true, false) => b.into(),
        (false, false) => format!("{a} · {b}"),
    }
}

fn hint(key: &str, action: &str, primary: bool) -> Hint {
    Hint {
        key: key.into(),
        action: action.into(),
        primary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_reads_in_megabytes_and_percent() {
        assert_eq!(
            progress(12_000_000, 300_000_000),
            "12.0 MB of 300.0 MB (4%)"
        );
        assert_eq!(progress(0, 0), "0.0 MB of 0.0 MB (100%)");
    }

    // One test for the shared state, so tests running side by side don't
    // see each other's transfers.
    #[tokio::test]
    async fn transfers_show_after_a_second_and_questions_wait_for_an_answer() {
        let t = begin(Way::FromMac, "a.iso", 10);
        assert!(!shown());
        assert!(badges("Right Ctrl", false).is_empty());
        RUNNING.lock().unwrap()[0].since -= QUIET;
        RUNNING.lock().unwrap()[0].shown.1 -= REDRAW;
        t.advance(5);
        let b = badges("Right Ctrl", true);
        assert_eq!(b.len(), 1);
        assert!(b[0].body.contains("a.iso from the Mac"));
        assert!(b[0].detail.contains("50%") && b[0].detail.contains("Right Ctrl"));
        assert!(shown());
        cancel();
        tokio::time::timeout(Duration::from_secs(1), t.cancelled())
            .await
            .expect("cancelled");
        drop(t);
        assert!(!shown());

        let asked = tokio::spawn(ask("big.iso (1 file(s), 1.2 GB)".into()));
        while !asking() {
            tokio::task::yield_now().await;
        }
        assert_eq!(badges("Right Ctrl", false)[0].hints[0].key, "Y");
        answer(true);
        assert!(asked.await.unwrap());
        assert!(!asking());
        // Aborted (a newer copy): the question goes too.
        let asked = tokio::spawn(ask("x".into()));
        while !asking() {
            tokio::task::yield_now().await;
        }
        asked.abort();
        let _ = asked.await;
        assert!(!asking());
    }
}
