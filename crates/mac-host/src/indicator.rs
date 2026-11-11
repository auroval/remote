//! An on-screen sign that a client is connected (#103): whoever sits at the
//! Mac sees it. On connect a coloured border goes round the screen for a few
//! seconds; then a small badge with the client's name stays in a corner for
//! the rest of the session. Both are click-through and left out of the
//! capture, so the client never sees them.
//!
//! AppKit windows need the main thread, which the agent's runtime holds, so
//! the windows live in a helper process: `auroval-agent _indicator <name>`.
//! The agent starts it per session ([`Indicator::start`]) and the capture
//! leaves out its windows ([`crate::capture::exclude_process`]). The helper
//! exits when its stdin closes, so it can't outlive the agent.
//!
//! With the curtain on (#144, [`crate::curtain`]) the same helper also
//! covers every display and holds back the Mac's own input; the border and
//! badge stay on top of it. Then stdin closing means the session ended: the
//! helper locks the screen and waits until it's locked before the cover
//! comes down. Only `release` on stdin, sent when nothing was ever shown to
//! a client, takes it down without locking.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, msg_send};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEventMask, NSFont,
    NSFontWeightSemibold, NSScreen, NSTextField, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString};

/// How long the border stays after connecting.
const BORDER_FOR: Duration = Duration::from_secs(4);
/// Border width in points.
const BORDER: f64 = 6.0;
/// The curtain: above normal windows, full-screen apps, the menu bar and
/// the Dock.
const CURTAIN_LEVEL: isize = 1000;
/// The border and badge, above the curtain.
const LEVEL: isize = CURTAIN_LEVEL + 1;
/// How long the helper waits for the screen to lock before taking the
/// curtain down anyway.
const LOCK_WAIT: Duration = Duration::from_secs(10);
/// How long the agent waits for the helper's windows before capturing
/// without them.
const READY_WAIT: Duration = Duration::from_secs(3);

/// The running helper; dropping it takes the indicator down.
pub struct Indicator {
    /// Taken when dropped.
    child: Option<Child>,
}

impl Indicator {
    /// Starts the helper for `client`, with the curtain if `curtain`, and
    /// waits until its windows are up, so the capture that starts next can
    /// leave them out.
    pub fn start(client: &str, curtain: bool) -> Result<Self, String> {
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let mut args = vec!["_indicator", client];
        if curtain {
            args.push("--curtain");
        }
        let mut child = Command::new(exe)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("starting it: {e}"))?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        let pid = child.id() as i32;
        let indicator = Self { child: Some(child) };
        match rx.recv_timeout(READY_WAIT) {
            Ok(line) if line.trim() == "ready" => {
                crate::capture::exclude_process(pid);
                Ok(indicator)
            }
            Ok(line) => Err(format!("it said {:?} instead of ready", line.trim())),
            Err(_) => Err(format!("not ready after {} s", READY_WAIT.as_secs())),
        }
    }
}

impl Indicator {
    /// Takes it down without locking: nothing reached a client.
    pub fn release(mut self) {
        if let Some(stdin) = self.child.as_mut().and_then(|c| c.stdin.as_mut()) {
            let _ = stdin.write_all(b"release\n");
            let _ = stdin.flush();
        }
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        crate::capture::include_process(child.id() as i32);
        // Closing stdin ends it (with the curtain: after locking). Waited
        // for on a thread of its own, so the agent goes on; killed only if
        // it hangs well past locking.
        drop(child.stdin.take());
        std::thread::spawn(move || {
            let deadline = Instant::now() + LOCK_WAIT + Duration::from_secs(2);
            while Instant::now() < deadline {
                if let Ok(Some(_)) = child.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            eprintln!("indicator: still running, killing it");
            let _ = child.kill();
            let _ = child.wait();
        });
    }
}

/// `auroval-agent _indicator <client name> [--curtain]`: shows the border
/// and badge (and the curtain) until stdin closes.
pub fn run(args: &[String]) -> Result<(), String> {
    let name = args
        .first()
        .ok_or("usage: auroval-agent _indicator <client name> [--curtain]")?;
    let curtain = args.iter().skip(1).any(|a| a == "--curtain");
    let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;

    let stop = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    {
        let (stop, released) = (stop.clone(), released.clone());
        std::thread::spawn(move || {
            // The agent holds the other end; EOF means it's done or gone.
            for line in std::io::stdin().lines() {
                match line {
                    Ok(l) if l.trim() == "release" => {
                        released.store(true, Ordering::Relaxed);
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            stop.store(true, Ordering::Relaxed);
        });
    }

    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon, no menu bar, never takes focus.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let screen = NSScreen::mainScreen(mtm).ok_or("no screen")?;
    let full = screen.frame();
    let colour = accent();
    let mut cover = curtain.then(|| crate::curtain::Cover::new(mtm, CURTAIN_LEVEL));
    let mut blackout = curtain.then(crate::curtain::Blackout::start);
    let border = border_windows(mtm, full, &colour);
    let badge = badge_window(mtm, screen.visibleFrame(), &colour, name);
    // All up before "ready", so the capture that starts next finds this
    // process on screen and leaves it out.
    if let Some(c) = &cover {
        c.show();
        match crate::curtain::hold_local_input() {
            Ok(()) => eprintln!("curtain: up, local input held back"),
            Err(e) => eprintln!("curtain: up, but local input isn't held back: {e}"),
        }
    }
    for w in border.iter().chain([&badge]) {
        w.orderFrontRegardless();
    }

    let started = Instant::now();
    let mut ready = false;
    let mut border_up = true;
    while !stop.load(Ordering::Relaxed) {
        pump(&app, 0.05);
        if !ready && started.elapsed() > Duration::from_millis(150) {
            // The window server has them; the agent can start capturing.
            println!("ready");
            let _ = std::io::stdout().flush();
            ready = true;
        }
        if border_up && started.elapsed() > BORDER_FOR {
            for w in &border {
                w.orderOut(None);
            }
            border_up = false;
        }
        if let Some(c) = &mut cover {
            c.follow_screens(mtm, CURTAIN_LEVEL);
        }
        if let Some(b) = &mut blackout {
            b.enforce();
        }
    }
    if cover.is_some() && !released.load(Ordering::Relaxed) {
        // The session ended (or the agent is gone): lock, and keep the
        // curtain until the lock screen is up.
        eprintln!("curtain: locking ({})", crate::lock::lock_screen());
        let deadline = Instant::now() + LOCK_WAIT;
        while !crate::curtain::screen_locked() && Instant::now() < deadline {
            pump(&app, 0.1);
            if let Some(b) = &mut blackout {
                b.enforce();
            }
        }
        if crate::curtain::screen_locked() {
            // Give the lock screen a moment to cover the displays.
            pump(&app, 0.5);
            eprintln!("curtain: locked, coming down");
        } else {
            eprintln!(
                "curtain: not locked after {} s, coming down anyway",
                LOCK_WAIT.as_secs()
            );
        }
    }
    // The displays' own colours back, then the cover down (when this
    // returns).
    drop(blackout);
    Ok(())
}

/// Handles the window server's events for up to `seconds`.
fn pump(app: &NSApplication, seconds: f64) {
    let until = NSDate::dateWithTimeIntervalSinceNow(seconds);
    while let Some(event) = unsafe {
        app.nextEventMatchingMask_untilDate_inMode_dequeue(
            NSEventMask::Any,
            Some(&until),
            NSDefaultRunLoopMode,
            true,
        )
    } {
        app.sendEvent(&event);
    }
}

/// Auroval's orange: the same in light and dark mode, and readable with
/// dark text on it.
fn accent() -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(1.0, 0.62, 0.04, 1.0)
}

/// A borderless, click-through window above everything, on every Space.
pub(crate) fn overlay_window(mtm: MainThreadMarker, frame: NSRect) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Borderless,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    unsafe { window.setReleasedWhenClosed(false) };
    window.setIgnoresMouseEvents(true);
    window.setLevel(LEVEL);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::Stationary
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::IgnoresCycle,
    );
    window.setHasShadow(false);
    window
}

/// Four thin windows along the screen's edges.
fn border_windows(mtm: MainThreadMarker, s: NSRect, colour: &NSColor) -> Vec<Retained<NSWindow>> {
    let (x, y, w, h) = (s.origin.x, s.origin.y, s.size.width, s.size.height);
    [
        (x, y + h - BORDER, w, BORDER), // top
        (x, y, w, BORDER),              // bottom
        (x, y, BORDER, h),              // left
        (x + w - BORDER, y, BORDER, h), // right
    ]
    .into_iter()
    .map(|(x, y, w, h)| {
        let window = overlay_window(mtm, rect(x, y, w, h));
        window.setBackgroundColor(Some(colour));
        window
    })
    .collect()
}

/// `● Auroval: <name>`, small, in the top right corner under the menu
/// bar.
fn badge_window(
    mtm: MainThreadMarker,
    visible: NSRect,
    colour: &NSColor,
    name: &str,
) -> Retained<NSWindow> {
    const MARGIN: f64 = 8.0;
    const PAD_X: f64 = 8.0;
    const PAD_Y: f64 = 2.0;
    let label = NSTextField::labelWithString(&NSString::from_str(&text(name)), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize_weight(11.0, unsafe {
        NSFontWeightSemibold
    })));
    label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.08, 0.08, 0.1, 1.0,
    )));
    label.sizeToFit();
    let text_size = label.frame().size;
    let (w, h) = (
        text_size.width + 2.0 * PAD_X,
        text_size.height + 2.0 * PAD_Y,
    );
    let x = visible.origin.x + visible.size.width - w - MARGIN;
    let y = visible.origin.y + visible.size.height - h - MARGIN;

    let window = overlay_window(mtm, rect(x, y, w, h));
    window.setOpaque(false);
    window.setBackgroundColor(Some(&NSColor::clearColor()));
    let content = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, w, h));
    content.setWantsLayer(true);
    // Rounded, filled with the accent: set on the layer, which saves
    // pulling in Core Animation's bindings for two properties.
    unsafe {
        let layer: *mut AnyObject = msg_send![&content, layer];
        if let Some(layer) = layer.as_ref() {
            let cg = colour.CGColor();
            let _: () = msg_send![layer, setBackgroundColor: &*cg];
            let _: () = msg_send![layer, setCornerRadius: h / 2.0];
        }
    }
    label.setFrameOrigin(NSPoint::new(PAD_X, PAD_Y));
    content.addSubview(&label);
    window.setContentView(Some(&content));
    window.setHasShadow(true);
    window.setAlphaValue(0.9);
    window
}

pub(crate) fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
}

/// The badge's text. Colour plus words: never colour alone.
fn text(name: &str) -> String {
    format!("● Auroval: {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badge_names_the_client() {
        assert_eq!(text("werkkamer"), "● Auroval: werkkamer");
    }
}
