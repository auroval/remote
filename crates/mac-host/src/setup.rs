//! The "Set up Auroval" window: the two permissions the host needs, one
//! button each, and a tick once macOS has it. The menu opens it when it
//! starts without them, and the agent asks for it when a client connects
//! before Screen Recording is on.
//!
//! macOS's own permission prompt appears once per app, ever: dismissed, or
//! shown while nobody looked, it doesn't come back. This window stays until
//! both are on, so nobody has to find the right pane in System Settings.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSFont, NSTextField, NSWindow, NSWindowStyleMask,
};
use objc2_core_graphics::{
    CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess, CGRequestPostEventAccess,
    CGRequestScreenCaptureAccess,
};
use objc2_foundation::{
    NSDistributedNotificationCenter, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString,
    NSTimer,
};

const SCREEN_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const ACCESSIBILITY_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Posted by the agent, heard by the menu: open the setup window.
const SHOW: &str = "nl.stormlantern.auroval.setup";

const WIDTH: f64 = 520.0;
const HEIGHT: f64 = 300.0;

pub struct Widgets {
    window: Retained<NSWindow>,
    accessibility: Retained<NSButton>,
    accessibility_done: Retained<NSTextField>,
    screen: Retained<NSButton>,
    screen_done: Retained<NSTextField>,
    footer: Retained<NSTextField>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    /// Both were on at the last tick.
    complete: Cell<bool>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Setup has no
    // Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AurovalSetup"]
    #[ivars = Widgets]
    pub struct Setup;

    impl Setup {
        #[unsafe(method(turnOnAccessibility:))]
        fn turn_on_accessibility(&self, _sender: Option<&AnyObject>) {
            // Puts Auroval in the list (switched off), and the first time
            // shows macOS's prompt; the pane is where the switch is.
            CGRequestPostEventAccess();
            crate::menu::open(ACCESSIBILITY_SETTINGS);
        }

        #[unsafe(method(turnOnScreen:))]
        fn turn_on_screen(&self, _sender: Option<&AnyObject>) {
            CGRequestScreenCaptureAccess();
            crate::menu::open(SCREEN_SETTINGS);
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: Option<&AnyObject>) {
            self.update();
        }

        #[unsafe(method(close:))]
        fn close(&self, _sender: Option<&AnyObject>) {
            self.ivars().window.close();
            self.stop_timer();
        }

        #[unsafe(method(showSetup:))]
        fn show_setup(&self, _note: Option<&AnyObject>) {
            self.show();
        }
    }

    unsafe impl NSObjectProtocol for Setup {}
);

impl Setup {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // Kept and shown again; AppKit mustn't free it on close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Set up Auroval"));
        let view = window.contentView().expect("a window has a content view");

        let label = |text: &str, bold: bool, x: f64, y: f64, w: f64, h: f64| {
            let field = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
            if bold {
                field.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            }
            field.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
            view.addSubview(&field);
            field
        };
        label(
            "Before your other computer can see and use this Mac, macOS needs two \
             permissions for Auroval. Click each button, then turn Auroval on in the \
             window that opens.",
            false,
            20.0,
            228.0,
            WIDTH - 40.0,
            52.0,
        );
        label("1. Accessibility", true, 20.0, 192.0, 300.0, 18.0);
        label(
            "Lets your other computer use this Mac's keyboard and mouse.",
            false,
            20.0,
            156.0,
            330.0,
            34.0,
        );
        label("2. Screen Recording", true, 20.0, 120.0, 300.0, 18.0);
        label(
            "Lets your other computer see this screen. If macOS offers to quit and \
             reopen Auroval, that's fine.",
            false,
            20.0,
            70.0,
            330.0,
            48.0,
        );
        let footer = label("", false, 20.0, 20.0, 360.0, 34.0);

        let this = Self::alloc(mtm).set_ivars(Widgets {
            accessibility: button(mtm, "Turn On…", 170.0),
            accessibility_done: done_label(mtm, 176.0),
            screen: button(mtm, "Turn On…", 98.0),
            screen_done: done_label(mtm, 104.0),
            footer,
            window,
            timer: RefCell::new(None),
            complete: Cell::new(false),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };

        let ivars = this.ivars();
        let target: &AnyObject = &this;
        for (b, action) in [
            (&ivars.accessibility, sel!(turnOnAccessibility:)),
            (&ivars.screen, sel!(turnOnScreen:)),
        ] {
            unsafe {
                b.setTarget(Some(target));
                b.setAction(Some(action));
            }
            view.addSubview(b);
        }
        view.addSubview(&ivars.accessibility_done);
        view.addSubview(&ivars.screen_done);
        let close = button(mtm, "Close", 16.0);
        unsafe {
            close.setTarget(Some(target));
            close.setAction(Some(sel!(close:)));
        }
        view.addSubview(&close);

        // The agent's request, from its own process.
        unsafe {
            NSDistributedNotificationCenter::defaultCenter().addObserver_selector_name_object(
                target,
                sel!(showSetup:),
                Some(&NSString::from_str(SHOW)),
                None,
            );
        }
        this
    }

    /// Brings the window up, in front of other apps, and keeps it current.
    pub fn show(&self) {
        let ivars = self.ivars();
        self.update();
        ivars.window.center();
        ivars.window.makeKeyAndOrderFront(None);
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
        if ivars.timer.borrow().is_none() {
            let target: &AnyObject = self;
            let timer = unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    1.0,
                    target,
                    sel!(tick:),
                    None,
                    true,
                )
            };
            *ivars.timer.borrow_mut() = Some(timer);
        }
    }

    fn stop_timer(&self) {
        if let Some(t) = self.ivars().timer.borrow_mut().take() {
            t.invalidate();
        }
    }

    fn update(&self) {
        let ivars = self.ivars();
        let accessibility = input_allowed();
        let screen = CGPreflightScreenCaptureAccess();
        ivars.accessibility.setHidden(accessibility);
        ivars.accessibility_done.setHidden(!accessibility);
        ivars.screen.setHidden(screen);
        ivars.screen_done.setHidden(!screen);
        let complete = accessibility && screen;
        ivars
            .footer
            .setStringValue(&NSString::from_str(if complete {
                "All set. You can connect from your other computer now."
            } else {
                "This window updates by itself; no restart needed."
            }));
        if complete && !ivars.complete.get() {
            // Back in front, so whoever is in System Settings sees it worked.
            ivars.window.orderFrontRegardless();
        }
        ivars.complete.set(complete);
    }
}

/// Whether macOS lets Auroval send keyboard and mouse events. Either
/// permission does: Accessibility (the switch people see and turn on) or
/// PostEvent (what `CGRequestPostEventAccess` asks for). Checking PostEvent
/// alone missed an Accessibility switch that was on, when an old PostEvent
/// entry from a differently signed build was still around.
pub fn input_allowed() -> bool {
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
    CGPreflightPostEventAccess() || unsafe { AXIsProcessTrusted() }
}

/// Whether either permission is missing, so the window should open.
pub fn needed() -> bool {
    !input_allowed() || !CGPreflightScreenCaptureAccess()
}

/// Asks the menu, in this user's session, to open the setup window; starts
/// the menu first if it isn't running (it opens the window by itself then).
pub fn request() {
    let uid = unsafe { libc::getuid() };
    let job = format!("gui/{uid}/{}.menu", crate::install::LABEL);
    // Without -k: does nothing when it runs already.
    let _ = std::process::Command::new("launchctl")
        .args(["kickstart", &job])
        .output();
    unsafe {
        NSDistributedNotificationCenter::defaultCenter()
            .postNotificationName_object_userInfo_deliverImmediately(
                &NSString::from_str(SHOW),
                None,
                None,
                true,
            );
    }
}

fn button(mtm: MainThreadMarker, title: &str, y: f64) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    };
    b.setFrame(NSRect::new(
        NSPoint::new(WIDTH - 140.0, y),
        NSSize::new(120.0, 32.0),
    ));
    b
}

fn done_label(mtm: MainThreadMarker, y: f64) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str("✓ On"), mtm);
    field.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
    field.setFrame(NSRect::new(
        NSPoint::new(WIDTH - 140.0, y),
        NSSize::new(120.0, 20.0),
    ));
    field
}
