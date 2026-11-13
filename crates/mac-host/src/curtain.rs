//! The curtain (#144): while a client is connected, an opaque cover over
//! every display at the Mac, and the Mac's own keyboard and mouse kept out
//! of the session. The client never sees the cover: it lives in the
//! indicator's helper process, which the capture leaves out (see
//! [`crate::indicator`]). When the session ends the screen is locked
//! before the cover comes down.
//!
//! The displays are also turned black through their gamma tables
//! ([`Blackout`]), as RustDesk does: the cover windows alone let the content
//! through for a moment while the window server animates a Space change
//! (entering or leaving full screen, cmd+tab). Gamma is applied on the way
//! to the panel, after the capture, so the client still sees the picture.
//! The cover stays as a second layer for the moments macOS resets the
//! gamma (after a display change); the blackout puts it back.
//!
//! Local input is held back by a session event tap that drops every event
//! not posted by our injector, which marks its events
//! ([`crate::inject::MARK`]).
//!
//! Known gaps: the pointer at the Mac still moves (the window server moves
//! it before any session tap sees the event; clicks and keys are dropped),
//! trackpad gestures the Dock handles itself (Mission Control swipes)
//! aren't stopped, and with the displays black, nothing at the Mac says
//! why: the cover's text and the badge are black too.

use std::cell::RefCell;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSColor, NSFont, NSScreen, NSTextAlignment, NSTextField, NSView, NSWindow};
use objc2_core_foundation::{
    CFBoolean, CFMachPort, CFRetained, CFRunLoop, CFString, kCFRunLoopCommonModes,
};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventTapProxy, CGEventType, CGSessionCopyCurrentDictionary,
};
use objc2_foundation::{NSRect, NSString, NSUserDefaults};

/// Shared by the menu (which sets it) and the agent (which reads it at the
/// start of each session). Not the app's bundle id: NSUserDefaults refuses
/// that as a suite.
const SUITE: &str = "nl.stormlantern.auroval";
const KEY: &str = "CurtainWhileConnected";

fn defaults() -> Option<Retained<NSUserDefaults>> {
    NSUserDefaults::initWithSuiteName(NSUserDefaults::alloc(), Some(&NSString::from_str(SUITE)))
}

/// Whether the curtain goes up for the next session. Off unless turned on.
pub fn enabled() -> bool {
    defaults().is_some_and(|d| d.boolForKey(&NSString::from_str(KEY)))
}

pub fn set_enabled(on: bool) {
    if let Some(d) = defaults() {
        d.setBool_forKey(on, &NSString::from_str(KEY));
    }
}

/// Whether this user's screen is locked right now.
pub fn screen_locked() -> bool {
    let Some(dict) = CGSessionCopyCurrentDictionary() else {
        return false;
    };
    let key = CFString::from_static_str("CGSSessionScreenIsLocked");
    let value = unsafe { dict.value((&*key as *const CFString).cast()) };
    // Absent when unlocked, kCFBooleanTrue when locked.
    !value.is_null() && value == (CFBoolean::new(true) as *const CFBoolean).cast()
}

/// The covers, one per display.
pub struct Cover {
    windows: Vec<Retained<NSWindow>>,
    frames: Vec<NSRect>,
}

impl Cover {
    pub fn new(mtm: MainThreadMarker, level: isize) -> Self {
        let frames = screen_frames(mtm);
        let windows = frames
            .iter()
            .map(|f| cover_window(mtm, *f, level))
            .collect();
        Self { windows, frames }
    }

    pub fn show(&self) {
        for w in &self.windows {
            w.orderFrontRegardless();
        }
    }

    /// Rebuilds the covers when a display was added, removed or resized.
    pub fn follow_screens(&mut self, mtm: MainThreadMarker, level: isize) {
        if screen_frames(mtm) != self.frames {
            println!("curtain: displays changed");
            for w in &self.windows {
                w.orderOut(None);
            }
            *self = Self::new(mtm, level);
            self.show();
        }
    }
}

fn screen_frames(mtm: MainThreadMarker) -> Vec<NSRect> {
    NSScreen::screens(mtm).iter().map(|s| s.frame()).collect()
}

fn cover_window(mtm: MainThreadMarker, frame: NSRect, level: isize) -> Retained<NSWindow> {
    let window = crate::indicator::overlay_window(mtm, frame);
    window.setLevel(level);
    window.setOpaque(true);
    window.setBackgroundColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.07, 0.07, 0.09, 1.0,
    )));
    let size = frame.size;
    let content = NSView::initWithFrame(
        NSView::alloc(mtm),
        crate::indicator::rect(0.0, 0.0, size.width, size.height),
    );
    let label =
        NSTextField::labelWithString(&NSString::from_str("This Mac is being used remotely."), mtm);
    label.setFont(Some(&NSFont::systemFontOfSize(22.0)));
    label.setTextColor(Some(&NSColor::colorWithSRGBRed_green_blue_alpha(
        0.85, 0.85, 0.88, 1.0,
    )));
    label.setAlignment(NSTextAlignment::Center);
    label.setFrame(crate::indicator::rect(
        0.0,
        size.height / 2.0 - 20.0,
        size.width,
        40.0,
    ));
    content.addSubview(&label);
    window.setContentView(Some(&content));
    window
}

/// The displays' gamma tables set to zero, so every display shows black.
/// Dropping it, or the process ending in any way, gives the displays their
/// own colours back (macOS restores ColorSync's tables when the process
/// that changed them exits).
pub struct Blackout {
    checked: Instant,
}

/// How often [`Blackout::enforce`] looks whether macOS reset a table.
const RECHECK: Duration = Duration::from_millis(200);

impl Blackout {
    pub fn start() -> Self {
        for d in online_displays() {
            if let Err(e) = black(d) {
                eprintln!("curtain: display {d} not blacked out: {e}");
            }
        }
        Self {
            checked: Instant::now(),
        }
    }

    /// Blacks out displays that came online since, or whose table macOS
    /// put back (it does after display changes). Cheap enough to call every
    /// turn of the run loop; does the work every [`RECHECK`].
    pub fn enforce(&mut self) {
        if self.checked.elapsed() < RECHECK {
            return;
        }
        self.checked = Instant::now();
        for d in online_displays() {
            if !is_black(d) {
                eprintln!("curtain: display {d} lit again, blacking it out");
                if let Err(e) = black(d) {
                    eprintln!("curtain: display {d} not blacked out: {e}");
                }
            }
        }
    }
}

impl Drop for Blackout {
    fn drop(&mut self) {
        unsafe { CGDisplayRestoreColorSyncSettings() };
    }
}

type DisplayId = u32;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetOnlineDisplayList(max: u32, displays: *mut DisplayId, count: *mut u32) -> i32;
    fn CGDisplayGammaTableCapacity(display: DisplayId) -> u32;
    fn CGSetDisplayTransferByTable(
        display: DisplayId,
        size: u32,
        red: *const f32,
        green: *const f32,
        blue: *const f32,
    ) -> i32;
    fn CGGetDisplayTransferByTable(
        display: DisplayId,
        capacity: u32,
        red: *mut f32,
        green: *mut f32,
        blue: *mut f32,
        count: *mut u32,
    ) -> i32;
    fn CGDisplayRestoreColorSyncSettings();
}

fn online_displays() -> Vec<DisplayId> {
    let mut ids = [0; 16];
    let mut count = 0;
    let err = unsafe { CGGetOnlineDisplayList(ids.len() as u32, ids.as_mut_ptr(), &mut count) };
    if err != 0 {
        eprintln!("curtain: no display list (error {err})");
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

fn black(display: DisplayId) -> Result<(), String> {
    let size = unsafe { CGDisplayGammaTableCapacity(display) };
    if size == 0 {
        return Err("no gamma table".into());
    }
    let zeros = vec![0.0f32; size as usize];
    let err = unsafe {
        CGSetDisplayTransferByTable(
            display,
            size,
            zeros.as_ptr(),
            zeros.as_ptr(),
            zeros.as_ptr(),
        )
    };
    if err == 0 {
        Ok(())
    } else {
        Err(format!("error {err}"))
    }
}

fn is_black(display: DisplayId) -> bool {
    let capacity = unsafe { CGDisplayGammaTableCapacity(display) };
    if capacity == 0 {
        // Nothing to set; don't retry every turn.
        return true;
    }
    let n = capacity as usize;
    let (mut r, mut g, mut b) = (vec![0.0f32; n], vec![0.0f32; n], vec![0.0f32; n]);
    let mut count = 0;
    let err = unsafe {
        CGGetDisplayTransferByTable(
            display,
            capacity,
            r.as_mut_ptr(),
            g.as_mut_ptr(),
            b.as_mut_ptr(),
            &mut count,
        )
    };
    err == 0 && all_dark(&[&r, &g, &b], count as usize)
}

fn all_dark(tables: &[&Vec<f32>], count: usize) -> bool {
    tables
        .iter()
        .all(|t| t.iter().take(count).all(|v| *v < 0.001))
}

thread_local! {
    /// The tap, so the callback can switch it back on after a timeout.
    static TAP: RefCell<Option<CFRetained<CFMachPort>>> = const { RefCell::new(None) };
}

/// The event types the Mac's own keyboard, mouse and trackpad produce.
const KINDS: &[u32] = &[
    1, 2, 3, 4, 5, 6, 7, // mouse buttons, moves, drags
    10, 11, 12, // keys, modifiers
    22, // scroll
    23, 24, // tablet
    25, 26, 27, // other mouse buttons
    18, 19, 20, 29, 30, 31, 32, // gestures (NSEvent types)
];

fn mask() -> u64 {
    KINDS.iter().fold(0, |m, k| m | (1u64 << k))
}

/// Starts dropping local input on the current (main) run loop. Needs
/// Accessibility; without it the cover still goes up, and this says why
/// input isn't held back.
pub fn hold_local_input() -> Result<(), String> {
    let tap = unsafe {
        CGEvent::tap_create(
            CGEventTapLocation::SessionEventTap,
            CGEventTapPlacement::HeadInsertEventTap,
            CGEventTapOptions::Default,
            mask(),
            Some(on_event),
            std::ptr::null_mut(),
        )
    }
    .ok_or("no event tap (Accessibility off?)")?;
    let source =
        CFMachPort::new_run_loop_source(None, Some(&tap), 0).ok_or("no run loop source")?;
    let run_loop = CFRunLoop::current().ok_or("no run loop")?;
    run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
    CGEvent::tap_enable(&tap, true);
    TAP.with(|t| *t.borrow_mut() = Some(tap));
    Ok(())
}

unsafe extern "C-unwind" fn on_event(
    _proxy: CGEventTapProxy,
    kind: CGEventType,
    event: NonNull<CGEvent>,
    _info: *mut c_void,
) -> *mut CGEvent {
    if kind == CGEventType::TapDisabledByTimeout || kind == CGEventType::TapDisabledByUserInput {
        TAP.with(|t| {
            if let Some(tap) = &*t.borrow() {
                CGEvent::tap_enable(tap, true);
            }
        });
        return event.as_ptr();
    }
    let ours = CGEvent::integer_value_field(
        Some(unsafe { event.as_ref() }),
        CGEventField::EventSourceUserData,
    ) == crate::inject::MARK;
    if ours {
        event.as_ptr()
    } else {
        std::ptr::null_mut()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_has_keys_and_clicks_but_not_tap_events() {
        let m = mask();
        assert_ne!(m & (1 << 10), 0); // key down
        assert_ne!(m & (1 << 1), 0); // left mouse down
        assert_eq!(m & (1 << 0), 0); // null event
    }

    #[test]
    fn dark_only_when_every_entry_is_zero() {
        let zero = vec![0.0; 4];
        let ramp = vec![0.0, 0.3, 0.6, 1.0];
        assert!(all_dark(&[&zero, &zero, &zero], 4));
        assert!(!all_dark(&[&zero, &ramp, &zero], 4));
        // Entries past the count don't matter.
        assert!(all_dark(&[&zero, &ramp, &zero], 1));
    }
}
