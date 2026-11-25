//! Drops files at a point of the Mac's screen, so the app under it gets an
//! ordinary drop, as if they were dragged there from Finder
//! (docs/PRD-file-transfer.md, F10; spike S, e in spikes/file-transfer).
//!
//! A mouse-down posted with `CGEventPost` never starts a drag. A drag the
//! process starts itself does: a small opaque window at the point, a
//! mouse-down made up for it and passed to `beginDraggingSession`, the
//! window then hidden so the drop lands on whatever is underneath, and the
//! pointer wiggled and let go there. The session ends with the operation
//! the app there chose, or none when nothing took the files.
//!
//! AppKit needs the main thread, which the agent's runtime holds (see
//! [`crate::indicator`]), so the drag runs in a helper process:
//! `auroval-agent _drop <x> <y> <path>…`. Its exit status says how it went.

use std::path::PathBuf;
use std::time::Duration;

use crate::input::{Point, Rect};

/// Exit status of the helper: something took the files.
const TAKEN: i32 = 0;
/// Exit status of the helper: the drag ended and nothing took them.
const NOT_TAKEN: i32 = 3;
/// Exit status of the helper: the drag never ended.
const NO_END: i32 = 4;

/// How long the agent waits for the helper before giving up on it.
const HELPER_WAIT: Duration = Duration::from_secs(12);

/// Where `at` (`0.0..=1.0` over the picture) is on `display`, in global
/// coordinates (points, from the top left of the main display), as the
/// injector maps the pointer.
pub fn screen_point(display: Rect, at: (f32, f32)) -> Point {
    crate::input::at(display, at.0, at.1)
}

/// `y` from the top of the main display, as Cocoa has it: from its bottom.
pub fn cocoa_y(y: f64, main_height: f64) -> f64 {
    main_height - y
}

/// The helper's arguments for a drop of `paths` at `p`.
pub fn helper_args(p: Point, paths: &[PathBuf]) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "_drop".into(),
        p.x.to_string().into(),
        p.y.to_string().into(),
    ];
    args.extend(paths.iter().map(|p| p.clone().into_os_string()));
    args
}

/// What the helper's exit status means for the client.
pub fn outcome(status: Option<i32>) -> Result<(), String> {
    match status {
        Some(TAKEN) => Ok(()),
        Some(NOT_TAKEN) => Err("nothing there took the files".into()),
        Some(NO_END) => Err("the drop on the Mac didn't finish".into()),
        Some(code) => Err(format!("the drop on the Mac failed (status {code})")),
        None => Err("the drop on the Mac was stopped".into()),
    }
}

/// Parses `<x> <y> <path>…`.
fn parse(args: &[String]) -> Result<(Point, Vec<PathBuf>), String> {
    const USAGE: &str = "usage: auroval-agent _drop <x> <y> <path>...";
    let [x, y, paths @ ..] = args else {
        return Err(USAGE.into());
    };
    let x: f64 = x.parse().map_err(|_| USAGE)?;
    let y: f64 = y.parse().map_err(|_| USAGE)?;
    if paths.is_empty() || !x.is_finite() || !y.is_finite() {
        return Err(USAGE.into());
    }
    Ok((Point { x, y }, paths.iter().map(PathBuf::from).collect()))
}

#[cfg(target_os = "macos")]
pub use mac::{drop_at, run};

#[cfg(target_os = "macos")]
mod mac {
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2::{
        AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    };
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSColor, NSDragOperation, NSDraggingContext,
        NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent, NSEventModifierFlags,
        NSEventType, NSImage, NSScreen, NSView,
    };
    use objc2_core_foundation::CGPoint;
    use objc2_core_graphics::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton};
    use objc2_foundation::{
        NSArray, NSObjectProtocol, NSPoint, NSProcessInfo, NSRect, NSString, NSURL,
    };

    use super::{HELPER_WAIT, NO_END, NOT_TAKEN, TAKEN, helper_args, outcome, parse};
    use crate::input::Point;

    /// The window's side, in points.
    const SIZE: f64 = 24.0;
    /// Above the curtain and the indicator ([`crate::indicator`]).
    const LEVEL: isize = 1002;
    /// When the posted press goes out, after the window is up.
    const PRESS_AT: Duration = Duration::from_millis(300);
    /// When the drag starts from a made-up press, if the posted one didn't
    /// reach the window (it doesn't, in practice).
    const FALLBACK_AT: Duration = Duration::from_millis(1000);
    /// Pointer moves after the drag started, a few points off and back, so
    /// AppKit sees a drag; the last one is the point itself.
    const WIGGLE: [f64; 6] = [2.0, 5.0, 8.0, 5.0, 2.0, 0.0];
    const WIGGLE_FROM: Duration = Duration::from_millis(100);
    const WIGGLE_STEP: Duration = Duration::from_millis(50);
    /// Let go this long after the last move.
    const RELEASE_AFTER: Duration = Duration::from_millis(300);
    /// The drag ends within ~250 ms of letting go (spike e); give up after.
    const END_WAIT: Duration = Duration::from_secs(5);
    /// Stay this long after the drag ended, while the app there reads
    /// what was dropped.
    const LINGER: f64 = 0.5;

    /// Drops `paths` at `at` (`0.0..=1.0` over the picture of `display`):
    /// `Ok` when something there took them. Blocks for the drag, a second
    /// or two.
    pub fn drop_at(display: u32, at: (f32, f32), paths: &[PathBuf]) -> Result<(), String> {
        let p = super::screen_point(crate::capture::bounds(display), at);
        let exe = std::env::current_exe().map_err(|e| format!("this binary: {e}"))?;
        let mut child = Command::new(exe)
            .args(helper_args(p, paths))
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("can't start the drop on the Mac: {e}"))?;
        let deadline = Instant::now() + HELPER_WAIT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return outcome(status.code()),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return outcome(Some(NO_END));
                }
                Err(e) => return Err(format!("the drop on the Mac: {e}")),
            }
        }
    }

    struct Ivars {
        urls: Vec<Retained<NSURL>>,
        started: Cell<bool>,
        /// The operation the drag ended with, once it ended.
        ended: Cell<Option<NSDragOperation>>,
    }

    define_class!(
        // SAFETY: NSView may be subclassed; DropView has no Drop.
        #[unsafe(super(NSView))]
        #[thread_kind = MainThreadOnly]
        #[name = "AurovalDropView"]
        #[ivars = Ivars]
        struct DropView;

        impl DropView {
            #[unsafe(method(acceptsFirstMouse:))]
            fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
                true
            }

            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, event: &NSEvent) {
                eprintln!("drop: the press reached the window");
                self.begin(event);
            }
        }

        unsafe impl NSObjectProtocol for DropView {}

        unsafe impl NSDraggingSource for DropView {
            #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
            fn source_operation_mask(
                &self,
                _session: &NSDraggingSession,
                _context: NSDraggingContext,
            ) -> NSDragOperation {
                NSDragOperation::Copy
            }

            #[unsafe(method(draggingSession:endedAtPoint:operation:))]
            fn ended(&self, _session: &NSDraggingSession, _at: NSPoint, operation: NSDragOperation) {
                self.ivars().ended.set(Some(operation));
            }
        }
    );

    impl DropView {
        fn new(mtm: MainThreadMarker, frame: NSRect, urls: Vec<Retained<NSURL>>) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(Ivars {
                urls,
                started: Cell::new(false),
                ended: Cell::new(None),
            });
            unsafe { msg_send![super(this), initWithFrame: frame] }
        }

        /// Starts the drag from `event`, once, and gets out of the way.
        fn begin(&self, event: &NSEvent) {
            if self.ivars().started.replace(true) {
                return;
            }
            let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                &NSString::from_str("doc"),
                None,
            );
            let items: Vec<Retained<NSDraggingItem>> = self
                .ivars()
                .urls
                .iter()
                .map(|url| {
                    let item = NSDraggingItem::initWithPasteboardWriter(
                        NSDraggingItem::alloc(),
                        ProtocolObject::from_ref(&**url),
                    );
                    let contents: Option<&AnyObject> = image.as_deref().map(|i| &***i);
                    // SAFETY: an NSImage is what the contents may be.
                    unsafe { item.setDraggingFrame_contents(self.bounds(), contents) };
                    item
                })
                .collect();
            let items = NSArray::from_retained_slice(&items);
            let session = self.beginDraggingSessionWithItems_event_source(
                &items,
                event,
                ProtocolObject::from_ref(self),
            );
            session.setAnimatesToStartingPositionsOnCancelOrFail(false);
            // So the drop lands on whatever is underneath.
            if let Some(window) = self.window() {
                window.setIgnoresMouseEvents(true);
                window.setAlphaValue(0.0);
            }
        }
    }

    /// Posts a left-button mouse event at `p`, marked as ours so a curtain
    /// lets it through.
    fn post(kind: CGEventType, p: Point) {
        let source = crate::inject::marked_source();
        let at = CGPoint { x: p.x, y: p.y };
        if let Some(event) =
            CGEvent::new_mouse_event(source.as_deref(), kind, at, CGMouseButton::Left)
        {
            CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        }
    }

    /// `auroval-agent _drop <x> <y> <path>…`: drops the paths at `x, y`
    /// (global, points from the top left of the main display) and exits
    /// with how it went.
    pub fn run(args: &[String]) -> Result<(), String> {
        let (p, paths) = parse(args)?;
        let mtm = MainThreadMarker::new().ok_or("not on the main thread")?;
        let app = NSApplication::sharedApplication(mtm);
        // No Dock icon, no menu bar, never takes focus.
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();

        let main_height = NSScreen::screens(mtm)
            .firstObject()
            .map(|s| s.frame().size.height)
            .ok_or("no screen")?;
        let urls = paths
            .iter()
            .map(|p| NSURL::fileURLWithPath(&NSString::from_str(&p.to_string_lossy())))
            .collect();
        // The pointer goes to the point first: the drag starts where it is.
        post(CGEventType::MouseMoved, p);

        let cy = super::cocoa_y(p.y, main_height);
        let frame = crate::indicator::rect(p.x - SIZE / 2.0, cy - SIZE / 2.0, SIZE, SIZE);
        let window = crate::indicator::overlay_window(mtm, frame);
        window.setIgnoresMouseEvents(false);
        window.setLevel(LEVEL);
        // Fully opaque: the window server lets clicks through (nearly)
        // transparent pixels.
        window.setOpaque(true);
        window.setBackgroundColor(Some(&NSColor::systemBlueColor()));
        let view = DropView::new(mtm, crate::indicator::rect(0.0, 0.0, SIZE, SIZE), urls);
        window.setContentView(Some(&view));
        window.orderFrontRegardless();

        let start = Instant::now();
        let mut pressed = false;
        let mut dragging: Option<Instant> = None;
        let mut moves = 0;
        let mut released: Option<Instant> = None;
        let status = loop {
            crate::indicator::pump(&app, 0.01);
            if let Some(op) = view.ivars().ended.get() {
                if op == NSDragOperation::None {
                    eprintln!("drop: ended, nothing took the files");
                    break NOT_TAKEN;
                }
                eprintln!("drop: ended, taken (operation {})", op.0);
                break TAKEN;
            }
            let now = start.elapsed();
            if !pressed && now >= PRESS_AT {
                post(CGEventType::LeftMouseDown, p);
                pressed = true;
            }
            if !view.ivars().started.get() && now >= FALLBACK_AT {
                let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                    NSEventType::LeftMouseDown,
                    NSPoint::new(SIZE / 2.0, SIZE / 2.0),
                    NSEventModifierFlags::empty(),
                    NSProcessInfo::processInfo().systemUptime(),
                    window.windowNumber(),
                    None,
                    0,
                    1,
                    1.0,
                );
                match event {
                    Some(event) => view.begin(&event),
                    None => {
                        eprintln!("drop: no press to start the drag from");
                        break NO_END;
                    }
                }
            }
            if view.ivars().started.get() && dragging.is_none() {
                dragging = Some(Instant::now());
            }
            if let Some(since) = dragging {
                let t = since.elapsed();
                while moves < WIGGLE.len() && t >= WIGGLE_FROM + WIGGLE_STEP * moves as u32 {
                    let d = WIGGLE[moves];
                    post(
                        CGEventType::LeftMouseDragged,
                        Point {
                            x: p.x + d,
                            y: p.y + d,
                        },
                    );
                    moves += 1;
                }
                let last = WIGGLE_FROM + WIGGLE_STEP * WIGGLE.len() as u32;
                if released.is_none() && t >= last + RELEASE_AFTER {
                    post(CGEventType::LeftMouseUp, p);
                    released = Some(Instant::now());
                }
            }
            if now >= FALLBACK_AT + END_WAIT * 2
                || released.is_some_and(|r| r.elapsed() >= END_WAIT)
            {
                eprintln!("drop: the drag didn't end");
                break NO_END;
            }
        };
        // Never leave the button down.
        if released.is_none() && pressed {
            post(CGEventType::LeftMouseUp, p);
        }
        crate::indicator::pump(&app, LINGER);
        window.orderOut(None);
        std::process::exit(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_point_is_on_the_display_in_global_points() {
        // A second display to the right of a 1512×982 main one.
        let display = Rect {
            x: 1512.0,
            y: 0.0,
            width: 2560.0,
            height: 1440.0,
        };
        assert_eq!(
            screen_point(display, (0.5, 0.25)),
            Point {
                x: 2792.0,
                y: 360.0
            }
        );
        // Out of range is the edge, as for the pointer.
        assert_eq!(
            screen_point(display, (1.5, -1.0)),
            Point { x: 4072.0, y: 0.0 }
        );
        assert_eq!(cocoa_y(100.0, 982.0), 882.0);
    }

    #[test]
    fn helper_arguments_roundtrip() {
        let p = Point { x: 12.5, y: 700.0 };
        let paths = vec![PathBuf::from("/tmp/a b.txt"), PathBuf::from("/tmp/map")];
        let args: Vec<String> = helper_args(p, &paths)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(args[0], "_drop");
        assert_eq!(parse(&args[1..]).unwrap(), (p, paths));
        assert!(parse(&args[1..3]).is_err());
        assert!(parse(&["x".into(), "1".into(), "/a".into()]).is_err());
        assert!(parse(&["NaN".into(), "1".into(), "/a".into()]).is_err());
    }

    #[test]
    fn the_helpers_status_says_whether_it_landed() {
        assert_eq!(outcome(Some(TAKEN)), Ok(()));
        assert_eq!(
            outcome(Some(NOT_TAKEN)),
            Err("nothing there took the files".into())
        );
        assert!(outcome(Some(NO_END)).is_err());
        assert!(outcome(Some(1)).is_err());
        assert!(outcome(None).is_err());
    }
}
