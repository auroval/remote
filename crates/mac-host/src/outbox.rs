//! Files copied on the Mac (in Finder), read and sent to the client for its
//! clipboard (docs/PRD-file-transfer.md, phase P2).
//!
//! Eager, like the other direction: the copy goes out when the user
//! copies, so the paste on Linux is instant and needs no round trip while a
//! file manager waits. Reading runs on a thread of its own: the first read
//! from Desktop, Documents or Downloads blocks until the user answers
//! macOS's folder-access prompt (spike S, d), and that must hold up nothing
//! else.
//!
//! The steps go to a `sink` (the daemon over IPC, or `serve`'s stream
//! writer), at most [`WINDOW`] chunks ahead of what the network took: the
//! side that writes the stream says [`Outbox::written`] for every chunk.
//! So a fast disk never piles a copy up in memory, and a slow network
//! slows the reading instead.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use auroval_proto::files::{
    ASK_ABOVE_BYTES, FileOffer, Incoming, MAX_FILES_BYTES, megabytes, scan, sizes, summary,
};

/// Biggest piece of a file passed on at once. Small, so a video frame
/// behind one on the agent's socket waits for little.
pub const CHUNK: usize = 64 * 1024;

/// Chunks passed on but not yet written to the network.
pub const WINDOW: usize = 16;

/// A running outbox. Dropping it stops the thread; a copy on its way is
/// cut off (the sink hears [`Incoming::Abort`]). Not waited for: a read
/// may hang on macOS's prompt until the user answers it.
pub struct Outbox {
    copies: Option<Sender<Vec<PathBuf>>>,
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    /// Chunks passed on and not yet written.
    in_flight: Mutex<usize>,
    room: Condvar,
    /// The client stopped the copy on its way (cancelled it).
    stopped: AtomicBool,
    quit: AtomicBool,
}

impl Outbox {
    /// `sink` gets each copy step by step and may block. `allowed` is asked
    /// before a copy is read (the screen may be locked by then). `failed`
    /// hears why a copy couldn't be sent, for the client (F9).
    pub fn start(
        sink: impl Fn(Incoming) + Send + 'static,
        allowed: impl Fn() -> Result<(), String> + Send + 'static,
        failed: impl Fn(String) + Send + 'static,
    ) -> Self {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared::default());
        {
            let shared = shared.clone();
            let started = std::thread::Builder::new()
                .name("outbox".into())
                .spawn(move || run(rx, &shared, sink, allowed, failed));
            if let Err(e) = started {
                eprintln!("files: no thread to send copies to the client: {e}");
            }
        }
        Self {
            copies: Some(tx),
            shared,
        }
    }

    /// Something that hands the outbox what the user copied, by path, from
    /// any thread. A newer copy replaces one still on its way.
    pub fn copier(&self) -> impl Fn(Vec<PathBuf>) + Send + 'static {
        let tx = self.copies.clone();
        move |paths| {
            if let Some(tx) = &tx {
                let _ = tx.send(paths);
            }
        }
    }

    /// One chunk is out on the network (or thrown away): one more may go.
    pub fn written(&self) {
        let mut n = self.shared.in_flight.lock().unwrap();
        *n = n.saturating_sub(1);
        self.shared.room.notify_all();
    }

    /// The client stopped the copy on its way: stop reading it.
    pub fn stopped(&self) {
        self.shared.stopped.store(true, Ordering::Relaxed);
        self.shared.room.notify_all();
    }
}

impl Drop for Outbox {
    fn drop(&mut self) {
        self.copies = None;
        self.shared.quit.store(true, Ordering::Relaxed);
        self.shared.room.notify_all();
    }
}

/// How a copy ended, when not complete.
enum Cut {
    /// A newer copy came: send that one.
    Newer(Vec<PathBuf>),
    /// The client stopped it, or the outbox is going away.
    Stopped,
    /// It can't be sent, and why.
    Failed(String),
}

fn run(
    rx: Receiver<Vec<PathBuf>>,
    shared: &Shared,
    sink: impl Fn(Incoming),
    allowed: impl Fn() -> Result<(), String>,
    failed: impl Fn(String),
) {
    let mut next = None;
    loop {
        let paths = match next.take() {
            Some(p) => p,
            None => match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(p) => p,
                Err(RecvTimeoutError::Timeout) if !shared.quit.load(Ordering::Relaxed) => continue,
                Err(_) => return,
            },
        };
        shared.stopped.store(false, Ordering::Relaxed);
        if let Err(why) = allowed() {
            println!("files: not sending a copy to the client: {why}");
            failed(why);
            continue;
        }
        match send(&paths, &rx, shared, &sink) {
            Ok(()) => {}
            Err(Cut::Newer(p)) => next = Some(p),
            Err(Cut::Stopped) => println!("files: the copy to the client stopped"),
            Err(Cut::Failed(why)) => {
                eprintln!("files: not sent to the client: {why}");
                failed(why);
            }
        }
    }
}

/// Sends one copy. Once it has begun, any end but a complete one is an
/// [`Incoming::Abort`] to the sink, so the client throws it away.
fn send(
    paths: &[PathBuf],
    rx: &Receiver<Vec<PathBuf>>,
    shared: &Shared,
    sink: &impl Fn(Incoming),
) -> Result<(), Cut> {
    let (offer, sources, skipped) = scan(paths).map_err(|e| Cut::Failed(named(&e, paths)))?;
    for s in &skipped {
        println!("files: left out {} (not a file or folder)", s.display());
    }
    check(&offer).map_err(Cut::Failed)?;
    let what = summary(&offer);
    println!("files → client: {what}");
    let started = Instant::now();
    sink(Incoming::Begin(offer.clone()));
    let sent = send_bytes(&offer, &sources, rx, shared, sink);
    match &sent {
        Ok(()) => {
            sink(Incoming::End);
            println!(
                "files: {what} read for the client in {:.1} s",
                started.elapsed().as_secs_f32()
            );
        }
        Err(Cut::Failed(why)) => sink(Incoming::Abort(why.clone())),
        Err(_) => sink(Incoming::Abort("stopped".into())),
    }
    sent
}

/// Whether the copy may go: something in it, and not so big that the user
/// should have been asked first. The Mac doesn't ask yet (F7 says in the
/// menu-bar item's notification), so it says no, and why.
fn check(offer: &FileOffer) -> Result<(), String> {
    let total = offer.total_bytes();
    if offer.entries.is_empty() {
        return Err("no files in the copy".into());
    }
    if total > ASK_ABOVE_BYTES.min(MAX_FILES_BYTES) {
        return Err(format!(
            "{} is more than the {} the Mac sends without asking, and it can't ask yet: \
             copy fewer files at a time",
            megabytes(total),
            megabytes(ASK_ABOVE_BYTES)
        ));
    }
    offer.check()
}

fn send_bytes(
    offer: &FileOffer,
    sources: &[PathBuf],
    rx: &Receiver<Vec<PathBuf>>,
    shared: &Shared,
    sink: &impl Fn(Incoming),
) -> Result<(), Cut> {
    let mut buf = vec![0u8; CHUNK];
    for (source, size) in sources.iter().zip(sizes(offer)) {
        let file =
            File::open(source).map_err(|e| Cut::Failed(named(&e, std::slice::from_ref(source))))?;
        let mut file = file.take(size);
        let mut left = size;
        while left > 0 {
            wait_for_room(rx, shared)?;
            let n = file
                .read(&mut buf)
                .map_err(|e| Cut::Failed(named(&e, std::slice::from_ref(source))))?;
            if n == 0 {
                return Err(Cut::Failed(format!(
                    "{} got shorter while it was sent",
                    name(source)
                )));
            }
            *shared.in_flight.lock().unwrap() += 1;
            sink(Incoming::Data(buf[..n].to_vec()));
            left -= n as u64;
        }
    }
    Ok(())
}

/// Waits until the network has room for another chunk; returns early when
/// the copy is cut off.
fn wait_for_room(rx: &Receiver<Vec<PathBuf>>, shared: &Shared) -> Result<(), Cut> {
    let mut n = shared.in_flight.lock().unwrap();
    loop {
        if shared.quit.load(Ordering::Relaxed) || shared.stopped.load(Ordering::Relaxed) {
            return Err(Cut::Stopped);
        }
        match rx.try_recv() {
            Ok(newer) => return Err(Cut::Newer(newer)),
            Err(TryRecvError::Disconnected) => return Err(Cut::Stopped),
            Err(TryRecvError::Empty) => {}
        }
        if *n < WINDOW {
            return Ok(());
        }
        n = shared
            .room
            .wait_timeout(n, Duration::from_millis(100))
            .unwrap()
            .0;
    }
}

/// What the user reads when a file can't be read: macOS's privacy rules
/// by name (spike S, d: a denied folder-access prompt), never a bare
/// "Operation not permitted".
fn named(e: &io::Error, paths: &[PathBuf]) -> String {
    let names: Vec<String> = paths.iter().map(|p| name(p)).collect();
    let names = names.join(", ");
    if e.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "macOS doesn't let Auroval read {names}. Allow it under System Settings → \
             Privacy & Security → Files & Folders, then copy again"
        )
    } else {
        format!("can't read {names}: {e}")
    }
}

fn name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::mpsc::sync_channel;

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("auroval-outbox-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn steps() -> (impl Fn(Incoming) + Send, Receiver<Incoming>) {
        let (tx, rx) = channel();
        (move |s| tx.send(s).unwrap(), rx)
    }

    fn next(rx: &Receiver<Incoming>) -> Incoming {
        rx.recv_timeout(Duration::from_secs(5)).expect("a step")
    }

    #[test]
    fn a_copy_goes_out_no_further_ahead_than_the_network() {
        let dir = tempdir("window");
        let big = vec![7u8; CHUNK * (WINDOW + 4)];
        fs::write(dir.join("big"), &big).unwrap();
        let (sink, rx) = steps();
        let outbox = Outbox::start(sink, || Ok(()), |why| panic!("{why}"));
        outbox.copier()(vec![dir.join("big")]);
        assert!(matches!(next(&rx), Incoming::Begin(o) if o.total_bytes() == big.len() as u64));
        let mut got = Vec::new();
        for _ in 0..WINDOW {
            match next(&rx) {
                Incoming::Data(d) => got.extend(d),
                other => panic!("{other:?}"),
            }
        }
        // Nothing more until the network took some.
        assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
        loop {
            outbox.written();
            match next(&rx) {
                Incoming::Data(d) => got.extend(d),
                Incoming::End => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, big);
        drop(outbox);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stopped_copy_is_aborted_and_a_newer_one_replaces_it() {
        let dir = tempdir("stop");
        fs::write(dir.join("a"), vec![1u8; CHUNK * (WINDOW + 1)]).unwrap();
        fs::write(dir.join("b"), b"bee").unwrap();
        let (sink, rx) = steps();
        let outbox = Outbox::start(sink, || Ok(()), |_| {});
        let copy = outbox.copier();
        copy(vec![dir.join("a")]);
        assert!(matches!(next(&rx), Incoming::Begin(_)));
        for _ in 0..WINDOW {
            assert!(matches!(next(&rx), Incoming::Data(_)));
        }
        outbox.stopped();
        assert!(matches!(next(&rx), Incoming::Abort(_)));

        copy(vec![dir.join("a")]);
        assert!(matches!(next(&rx), Incoming::Begin(_)));
        // The window is still full from the first: a newer copy cuts in.
        copy(vec![dir.join("b")]);
        loop {
            match next(&rx) {
                Incoming::Data(_) => outbox.written(),
                Incoming::Abort(_) => break,
                other => panic!("{other:?}"),
            }
        }
        for _ in 0..WINDOW * 3 {
            outbox.written();
        }
        assert!(matches!(next(&rx), Incoming::Begin(o) if o.entries[0].path == ["b"]));
        assert!(matches!(next(&rx), Incoming::Data(d) if d == b"bee"));
        assert!(matches!(next(&rx), Incoming::End));
        drop(outbox);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_cant_be_sent_is_said_and_never_begun() {
        let (sink, rx) = steps();
        let (why_tx, why) = sync_channel(4);
        let outbox = Outbox::start(sink, || Err("the Mac's screen is locked".into()), {
            let why_tx = why_tx.clone();
            move |w| why_tx.send(w).unwrap()
        });
        outbox.copier()(vec![PathBuf::from("/nonexistent")]);
        let said = why.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(said.contains("locked"), "{said}");
        drop(outbox);

        let outbox = Outbox::start(steps().0, || Ok(()), move |w| why_tx.send(w).unwrap());
        outbox.copier()(vec![PathBuf::from("/nonexistent/thing.txt")]);
        let said = why.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(said.contains("can't read thing.txt"), "{said}");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_privacy_refusal_names_the_setting() {
        // EPERM, as macOS gives it for a denied folder (1 on both systems).
        let e = io::Error::from_raw_os_error(1);
        let said = named(&e, &[PathBuf::from("/Users/x/Downloads/report.pdf")]);
        assert!(
            said.contains("report.pdf") && said.contains("Files & Folders"),
            "{said}"
        );
    }

    #[test]
    fn copies_over_the_limit_for_asking_are_refused() {
        use auroval_proto::files::{FileEntry, FileKind};
        let offer = FileOffer {
            entries: vec![FileEntry {
                path: vec!["big.iso".into()],
                kind: FileKind::File {
                    size: ASK_ABOVE_BYTES + 1,
                },
                modified_s: None,
            }],
            ..Default::default()
        };
        let why = check(&offer).unwrap_err();
        assert!(why.contains("500.0 MB"), "{why}");
    }
}
