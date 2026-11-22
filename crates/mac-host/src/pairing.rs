//! Pairing from the menu (#141), without Terminal.
//!
//! The paired clients are root's, so the menu (the logged-in user) can't
//! open pairing itself. It runs `auroval-agent _pair-for UID TOKEN` through
//! macOS's administrator password dialog, as `sudo auroval-agent pair` would
//! be. That helper opens pairing and keeps one status line in
//! `/var/run/auroval/pair-UID`, a file only that user and root can read:
//! the code while it's open, then how it ended. The menu's window reads it,
//! and writes `cancel` into it when closed early.
//!
//! The token, fresh for every request, tells the window which run a line
//! belongs to, so a line left from an earlier run is never shown.

/// What the helper reports, one line per state.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Open: type this code before `expires` (Unix seconds).
    Open {
        code: String,
        expires: u64,
    },
    Paired(String),
    Expired,
    TriedOut,
    /// The window was closed before a client paired; pairing is closed.
    Cancelled,
    Failed(String),
    /// Written by the window: close pairing.
    Cancel,
}

pub fn format(token: &str, status: &Status) -> String {
    let rest = match status {
        Status::Open { code, expires } => format!("open {code} {expires}"),
        Status::Paired(name) => format!("paired {}", one_line(name)),
        Status::Expired => "expired".into(),
        Status::TriedOut => "tried-out".into(),
        Status::Cancelled => "cancelled".into(),
        Status::Failed(why) => format!("failed {}", one_line(why)),
        Status::Cancel => "cancel".into(),
    };
    format!("{token} {rest}\n")
}

/// The status in `text`, if it's from the run with this token.
pub fn parse(token: &str, text: &str) -> Option<Status> {
    let line = text.lines().next()?;
    let (t, rest) = line.split_once(' ')?;
    if t != token {
        return None;
    }
    let (word, arg) = rest.split_once(' ').unwrap_or((rest, ""));
    Some(match word {
        "open" => {
            let (code, expires) = arg.split_once(' ')?;
            Status::Open {
                code: code.into(),
                expires: expires.parse().ok()?,
            }
        }
        "paired" => Status::Paired(arg.into()),
        "expired" => Status::Expired,
        "tried-out" => Status::TriedOut,
        "cancelled" => Status::Cancelled,
        "failed" => Status::Failed(arg.into()),
        "cancel" => Status::Cancel,
        _ => return None,
    })
}

/// Tokens are hex, so they're safe on a command line and in the file.
pub fn valid_token(token: &str) -> bool {
    (16..=64).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(target_os = "macos")]
pub use mac::*;

#[cfg(target_os = "macos")]
mod mac {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    use super::{Status, format, parse, valid_token};
    use crate::trust::{PAIRING_TRIES, Store};

    pub fn status_path(uid: u32) -> PathBuf {
        PathBuf::from(format!("/var/run/auroval/pair-{uid}"))
    }

    /// A fresh token for one request.
    pub fn new_token() -> String {
        let mut bytes = [0u8; 16];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = std::io::Read::read_exact(&mut f, &mut bytes);
        }
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The window's side: asks for the administrator password and starts
    /// the helper. Blocks while the password dialog is up. `Err(None)`:
    /// the user cancelled the dialog.
    pub fn request(token: &str) -> Result<(), Option<String>> {
        let exe = std::env::current_exe().map_err(|e| Some(e.to_string()))?;
        let uid = unsafe { libc::getuid() };
        // In the background, so the dialog's script returns at once; the
        // helper reports through the status file.
        let command = format!(
            "'{}' _pair-for {uid} {token} >/dev/null 2>&1 &",
            exe.display()
        );
        let script = format!(
            "do shell script \"{}\" with prompt \"Auroval wants to let a new computer connect \
             to this Mac.\" with administrator privileges",
            command.replace('\\', "\\\\").replace('"', "\\\"")
        );
        let out = Command::new("osascript")
            .args(["-e", &script])
            .output()
            .map_err(|e| Some(format!("osascript: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr);
        // -128: "User canceled."
        if err.contains("-128") {
            Err(None)
        } else {
            Err(Some(err.trim().to_string()))
        }
    }

    /// The window's view of the run with this token, if it reported yet.
    pub fn read(token: &str) -> Option<Status> {
        let uid = unsafe { libc::getuid() };
        parse(token, &std::fs::read_to_string(status_path(uid)).ok()?)
    }

    /// The window was closed early: ask the helper to close pairing.
    pub fn cancel(token: &str) {
        let uid = unsafe { libc::getuid() };
        // Never creates: the folder is root's, and the helper made the file.
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(status_path(uid))
        {
            let _ = f.write_all(format(token, &Status::Cancel).as_bytes());
        }
    }

    /// `auroval-agent _pair-for UID TOKEN`, as root: open pairing and report
    /// to that user until a client paired, the code expired or was tried
    /// out, or the window was closed.
    pub fn run_for(args: &[String]) -> Result<(), String> {
        let [uid, token] = args else {
            return Err("usage: auroval-agent _pair-for UID TOKEN".into());
        };
        let uid: u32 = uid.parse().map_err(|e| format!("UID: {e}"))?;
        if !valid_token(token) {
            return Err("TOKEN: expected hex".into());
        }
        if unsafe { libc::geteuid() } != 0 {
            return Err("needs root".into());
        }
        let path = status_path(uid);
        // Fresh, private to that user: nobody else may read the code.
        let _ = std::fs::remove_file(&path);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        std::os::unix::fs::fchown(&file, Some(uid), None)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        drop(file);
        let report = |s: &Status| {
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
            {
                let _ = f.write_all(format(token, s).as_bytes());
            }
        };

        let store = Store::system();
        let ended = watch(&store, token, &path, &report);
        if let Err(why) = &ended {
            report(&Status::Failed(why.clone()));
        }
        ended
    }

    fn watch(
        store: &Store,
        token: &str,
        path: &std::path::Path,
        report: &dyn Fn(&Status),
    ) -> Result<(), String> {
        store
            .identity()
            .map_err(|e| format!("this Mac's identity: {e}"))?;
        let before = store.clients_changed();
        let pairing = store.open_pairing().map_err(|e| e.to_string())?;
        report(&Status::Open {
            code: pairing.code.clone(),
            expires: pairing.expires,
        });
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let wanted = std::fs::read_to_string(path).ok();
            if wanted.as_deref().and_then(|t| parse(token, t)) == Some(Status::Cancel) {
                store.close_pairing();
                report(&Status::Cancelled);
                return Ok(());
            }
            if store.clients_changed() != before {
                // The one just paired is last, also when it was paired before.
                let now = store
                    .clients()
                    .map_err(|e| format!("paired clients: {e}"))?;
                let name = now.last().map_or_else(String::new, |c| c.name.clone());
                store.close_pairing();
                report(&Status::Paired(name));
                return Ok(());
            }
            if store.pairing().is_none() {
                // The daemon removes the file after the last wrong code; an
                // expired one stays until closed.
                let tried_out = !store.dir().join("pairing").exists();
                store.close_pairing();
                report(if tried_out {
                    &Status::TriedOut
                } else {
                    &Status::Expired
                });
                return Ok(());
            }
        }
    }

    pub const TRIES: u32 = PAIRING_TRIES;
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "00112233445566778899aabbccddeeff";

    #[test]
    fn every_status_reads_back() {
        for s in [
            Status::Open {
                code: "123456".into(),
                expires: 1_800_000_000,
            },
            Status::Paired("games pc".into()),
            Status::Expired,
            Status::TriedOut,
            Status::Cancelled,
            Status::Failed("no identity".into()),
            Status::Cancel,
        ] {
            assert_eq!(parse(T, &format(T, &s)), Some(s));
        }
    }

    #[test]
    fn a_line_from_another_run_is_ignored() {
        let other = "ffeeddccbbaa99887766554433221100";
        assert_eq!(parse(T, &format(other, &Status::Expired)), None);
        assert_eq!(parse(T, ""), None);
    }

    #[test]
    fn a_name_stays_on_one_line() {
        let line = format(T, &Status::Paired("evil\nname".into()));
        assert_eq!(line.lines().count(), 1);
        assert_eq!(parse(T, &line), Some(Status::Paired("evil name".into())));
    }

    #[test]
    fn tokens_are_hex_only() {
        assert!(valid_token(T));
        assert!(!valid_token("short"));
        assert!(!valid_token("00112233445566778899aabbccddeeff; rm -rf /"));
    }
}
