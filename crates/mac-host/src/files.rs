//! Files the client copied, written to a temporary folder of this process
//! and put on the Mac's clipboard from there (docs/PRD-file-transfer.md).
//!
//! Runs where the pasteboard is: in the agent of a user session, or in
//! `serve`. Writing happens on a thread of its own, so a big copy never
//! holds up input. Everything is removed when the session ends (and
//! leftovers of a crashed run at the next start): pasted files are Finder's
//! own copies by then.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

use auroval_proto::files::{Incoming, Writer};

/// Completed copies kept on disk: the one on the clipboard and the one
/// before, which a slow paste may still be reading.
const KEEP: usize = 2;

/// A running inbox. Dropping it stops the thread and removes its folder.
pub struct Inbox {
    tx: Option<Sender<Incoming>>,
    thread: Option<JoinHandle<()>>,
}

impl Inbox {
    /// `done` gets the top-level paths of every complete copy, for the
    /// clipboard. `allowed` is asked when a copy begins (the screen may be
    /// locked by then).
    pub fn start(
        done: impl Fn(Vec<PathBuf>) + Send + 'static,
        allowed: impl Fn() -> Result<(), String> + Send + 'static,
    ) -> Self {
        let base = base_dir();
        clean_leftovers(&base);
        let (tx, rx) = channel();
        let thread = std::thread::Builder::new()
            .name("files".into())
            .spawn(move || {
                run(&base, rx, done, allowed);
                let _ = std::fs::remove_dir_all(&base);
            })
            .ok();
        Self {
            tx: Some(tx),
            thread,
        }
    }

    pub fn handle(&self, msg: Incoming) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
    }
}

impl Drop for Inbox {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// `$TMPDIR/auroval-files/<pid>`: the user's own temp folder, which only
/// they can read; one folder per process, as agent and serve may both run.
fn base_dir() -> PathBuf {
    std::env::temp_dir()
        .join("auroval-files")
        .join(std::process::id().to_string())
}

/// Removes the folders of processes that are gone.
fn clean_leftovers(base: &Path) {
    let Some(parent) = base.parent() else { return };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for e in entries.flatten() {
        let gone = e
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
            .is_some_and(|pid| !alive(pid));
        if gone {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

fn alive(pid: i32) -> bool {
    // Signal 0 checks without sending; EPERM means it's someone else's.
    let sent = unsafe { libc::kill(pid, 0) } == 0;
    sent || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn run(
    base: &Path,
    rx: Receiver<Incoming>,
    done: impl Fn(Vec<PathBuf>),
    allowed: impl Fn() -> Result<(), String>,
) {
    let mut next = 0u64;
    let mut current: Option<Writer> = None;
    let mut kept: Vec<PathBuf> = Vec::new();
    let mut bytes = 0u64;
    let drop_current = |w: Option<Writer>| {
        if let Some(w) = w {
            let _ = std::fs::remove_dir_all(w.dir());
        }
    };
    while let Ok(msg) = rx.recv() {
        match msg {
            Incoming::Begin(offer) => {
                drop_current(current.take());
                if let Err(why) = allowed() {
                    println!("files: not taking a copy from the client: {why}");
                    continue;
                }
                next += 1;
                let dir = base.join(next.to_string());
                let made = std::fs::create_dir_all(&dir).and_then(|()| Writer::new(&dir, &offer));
                match made {
                    Ok(w) => {
                        println!(
                            "files ← client: {} file(s), {:.1} MB",
                            offer.files(),
                            offer.total_bytes() as f64 / 1e6
                        );
                        bytes = 0;
                        current = Some(w);
                    }
                    Err(e) => {
                        eprintln!("files: can't take the client's copy: {e}");
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                }
            }
            Incoming::Data(data) => {
                if let Some(w) = &mut current {
                    bytes += data.len() as u64;
                    if let Err(e) = w.write(&data) {
                        eprintln!("files: writing the client's copy failed: {e}");
                        drop_current(current.take());
                    }
                }
            }
            Incoming::End => {
                let Some(w) = current.take() else { continue };
                let dir = w.dir().to_path_buf();
                match w.finish() {
                    Ok(roots) => {
                        println!(
                            "files: {:.1} MB from the client on the clipboard",
                            bytes as f64 / 1e6
                        );
                        kept.push(dir);
                        while kept.len() > KEEP {
                            let _ = std::fs::remove_dir_all(kept.remove(0));
                        }
                        done(roots);
                    }
                    Err(e) => {
                        eprintln!("files: the client's copy is incomplete: {e}");
                        let _ = std::fs::remove_dir_all(&dir);
                    }
                }
            }
            Incoming::Abort(why) => {
                if current.is_some() {
                    println!("files: the client's copy stopped: {why}");
                }
                drop_current(current.take());
            }
        }
    }
    drop_current(current);
}
