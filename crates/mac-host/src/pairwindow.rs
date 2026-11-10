//! The "Pair a Client" window (#141): asks for the administrator password,
//! then shows the code, a countdown and what to type on Linux, and how it
//! ended. The work is done by the root helper in [`crate::pairing`].

use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSFont, NSTextAlignment, NSTextField, NSWindow,
    NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer};

use crate::pairing::{self, Status};

const WIDTH: f64 = 480.0;
const HEIGHT: f64 = 320.0;

/// The password dialog's answer, filled in by its thread.
type Asked = Arc<Mutex<Option<Result<(), Option<String>>>>>;

#[derive(Clone, PartialEq)]
enum Phase {
    /// The password dialog is up.
    Asking,
    /// The helper runs; pairing is open (or about to be).
    Open,
    /// Over: paired, expired, cancelled or failed.
    Done,
}

pub struct Widgets {
    window: Retained<NSWindow>,
    how: Retained<NSTextField>,
    code: Retained<NSTextField>,
    status: Retained<NSTextField>,
    again: Retained<NSButton>,
    timer: RefCell<Option<Retained<NSTimer>>>,
    token: RefCell<String>,
    phase: RefCell<Phase>,
    asked: RefCell<Asked>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and PairWindow has
    // no Drop.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "AurovalPairWindow"]
    #[ivars = Widgets]
    pub struct PairWindow;

    impl PairWindow {
        #[unsafe(method(showPairing:))]
        fn show_pairing(&self, _sender: Option<&AnyObject>) {
            if self.ivars().window.isVisible() {
                self.front();
            } else {
                self.start();
            }
        }

        #[unsafe(method(tryAgain:))]
        fn try_again(&self, _sender: Option<&AnyObject>) {
            self.start();
        }

        #[unsafe(method(close:))]
        fn close(&self, _sender: Option<&AnyObject>) {
            let ivars = self.ivars();
            if *ivars.phase.borrow() == Phase::Open {
                pairing::cancel(&ivars.token.borrow());
            }
            *ivars.phase.borrow_mut() = Phase::Done;
            self.stop_timer();
            ivars.window.close();
        }

        #[unsafe(method(tick:))]
        fn tick(&self, _timer: Option<&AnyObject>) {
            self.update();
        }
    }

    unsafe impl NSObjectProtocol for PairWindow {}
);

impl PairWindow {
    pub fn new(mtm: MainThreadMarker) -> Retained<Self> {
        // Only Close (below) closes it, so closing always closes pairing.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT)),
                NSWindowStyleMask::Titled,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Pair a Client"));
        let view = window.contentView().expect("a window has a content view");

        let label = |x: f64, y: f64, w: f64, h: f64| {
            let field = NSTextField::wrappingLabelWithString(&NSString::from_str(""), mtm);
            field.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
            field.setSelectable(true);
            view.addSubview(&field);
            field
        };
        let how = label(20.0, 160.0, WIDTH - 40.0, 140.0);
        let code = label(20.0, 100.0, WIDTH - 40.0, 52.0);
        code.setFont(Some(&NSFont::boldSystemFontOfSize(40.0)));
        code.setAlignment(NSTextAlignment::Center);
        let status = label(20.0, 60.0, WIDTH - 40.0, 34.0);
        status.setAlignment(NSTextAlignment::Center);

        let again = button(mtm, "Try Again", WIDTH - 260.0);
        let close = button(mtm, "Close", WIDTH - 140.0);
        let this = Self::alloc(mtm).set_ivars(Widgets {
            window,
            how,
            code,
            status,
            again,
            timer: RefCell::new(None),
            token: RefCell::new(String::new()),
            phase: RefCell::new(Phase::Done),
            asked: RefCell::new(Arc::new(Mutex::new(None))),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let target: &AnyObject = &this;
        for (b, action) in [
            (&this.ivars().again, sel!(tryAgain:)),
            (&close, sel!(close:)),
        ] {
            unsafe {
                b.setTarget(Some(target));
                b.setAction(Some(action));
            }
            view.addSubview(b);
        }
        this
    }

    /// A new request: the password dialog, then a new code.
    fn start(&self) {
        let ivars = self.ivars();
        let token = pairing::new_token();
        *ivars.token.borrow_mut() = token.clone();
        *ivars.phase.borrow_mut() = Phase::Asking;
        let asked: Asked = Arc::new(Mutex::new(None));
        *ivars.asked.borrow_mut() = asked.clone();
        // The dialog blocks until answered; the menu keeps running.
        std::thread::spawn(move || {
            let answer = pairing::request(&token);
            *asked.lock().unwrap() = Some(answer);
        });

        ivars
            .how
            .setStringValue(&NSString::from_str(&instructions()));
        self.update();
        self.front();
        if ivars.timer.borrow().is_none() {
            let target: &AnyObject = self;
            let timer = unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    0.5,
                    target,
                    sel!(tick:),
                    None,
                    true,
                )
            };
            *ivars.timer.borrow_mut() = Some(timer);
        }
    }

    fn front(&self) {
        let window = &self.ivars().window;
        window.center();
        window.makeKeyAndOrderFront(None);
        // A menu-bar app's request to come forward is often ignored (and
        // after the password dialog macOS hands focus back to the app that
        // had it), so put the window on top regardless.
        window.orderFrontRegardless();
        #[allow(deprecated)]
        NSApplication::sharedApplication(self.mtm()).activateIgnoringOtherApps(true);
    }

    fn stop_timer(&self) {
        if let Some(t) = self.ivars().timer.borrow_mut().take() {
            t.invalidate();
        }
    }

    fn update(&self) {
        let ivars = self.ivars();
        let phase = ivars.phase.borrow().clone();
        let (code, status, again) = match phase {
            Phase::Asking => match ivars.asked.borrow().lock().unwrap().clone() {
                None => (
                    String::new(),
                    "Enter your password to open pairing.".into(),
                    false,
                ),
                Some(Ok(())) => {
                    *ivars.phase.borrow_mut() = Phase::Open;
                    // Back on top after the password dialog.
                    self.front();
                    (String::new(), "Opening pairing…".into(), false)
                }
                Some(Err(None)) => {
                    *ivars.phase.borrow_mut() = Phase::Done;
                    (String::new(), "Pairing wasn't opened.".into(), true)
                }
                Some(Err(Some(why))) => {
                    *ivars.phase.borrow_mut() = Phase::Done;
                    (String::new(), format!("Pairing couldn't open: {why}"), true)
                }
            },
            Phase::Open => match pairing::read(&ivars.token.borrow()) {
                None | Some(Status::Cancel) => (String::new(), "Opening pairing…".into(), false),
                Some(Status::Open { code, expires }) => {
                    let left = expires.saturating_sub(now());
                    (
                        format!("{} {}", &code[..3], &code[3..]),
                        format!(
                            "Type this code on the other computer. {}:{:02} left.",
                            left / 60,
                            left % 60
                        ),
                        false,
                    )
                }
                Some(end) => {
                    *ivars.phase.borrow_mut() = Phase::Done;
                    self.stop_timer();
                    match end {
                        Status::Paired(name) => (
                            "✓".into(),
                            format!("Paired with {name}. It can connect now."),
                            false,
                        ),
                        Status::Expired => (String::new(), "The code expired.".into(), true),
                        Status::TriedOut => (
                            String::new(),
                            format!("{} wrong codes; pairing is closed.", pairing::TRIES),
                            true,
                        ),
                        Status::Failed(why) => {
                            (String::new(), format!("Pairing stopped: {why}"), true)
                        }
                        _ => (String::new(), "Pairing is closed.".into(), true),
                    }
                }
            },
            Phase::Done => return,
        };
        ivars.code.setStringValue(&NSString::from_str(&code));
        ivars.status.setStringValue(&NSString::from_str(&status));
        ivars.again.setHidden(!again);
    }
}

fn instructions() -> String {
    let mut text = String::from(
        "On the computer that should connect to this Mac, run:\n\n    auroval pair\n\n\
         It finds this Mac on the local network.",
    );
    let addresses = crate::session::local_addresses();
    if !addresses.is_empty() {
        let list: Vec<String> = addresses
            .iter()
            .map(|(_, ip)| format!("auroval pair {ip}"))
            .collect();
        text.push_str(&format!(" If it doesn't, run {}.", list.join(" or ")));
    }
    text
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn button(mtm: MainThreadMarker, title: &str, x: f64) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
    };
    b.setFrame(NSRect::new(NSPoint::new(x, 16.0), NSSize::new(120.0, 32.0)));
    b
}
