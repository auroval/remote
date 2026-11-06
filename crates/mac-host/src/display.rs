//! The `match` display mode (#111): the Mac's display switches to the mode
//! that best fits the client's window, so the picture goes out 1:1 instead
//! of scaled down, and back to its own mode when the session ends.
//!
//! Restoring is the part that matters, so it happens three ways:
//!
//! 1. [`Matcher`] puts the original mode back when told to and when it's
//!    dropped (the client left, switched to `fit`, or the daemon stopped
//!    the session or went away).
//! 2. The switch is made with `kCGConfigureForAppOnly`: the window server
//!    itself reverts it when the agent's process ends, however it ends
//!    (crash, SIGKILL). `kCGConfigureForSession` would keep it until logout.
//! 3. Before switching, the original mode is noted in a marker file; the
//!    next agent to start finds it and switches back ([`recover`]), in case
//!    the window server didn't. Like [`crate::mute`] and [`crate::output`].
//!
//! Only in a user session: the login window's agent stays at `fit` (it runs
//! as root before anyone logs in, and a mode set there would carry into the
//! login). A locked session is still the user's session and keeps matching.

/// A display mode, as far as choosing one goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    /// Size in points: what windows are laid out in.
    pub width: u32,
    pub height: u32,
    /// Size in pixels: what's captured. Twice the points for a HiDPI mode.
    pub pixel_width: u32,
    pub pixel_height: u32,
    /// Refresh rate in millihertz; 0 when the display doesn't say.
    pub refresh_mhz: u32,
    /// The mode's IOKit id, to find it again.
    pub id: i32,
}

impl Mode {
    pub fn hidpi(&self) -> bool {
        self.pixel_width > self.width
    }

    #[cfg(test)]
    pub fn pixels(&self) -> (u32, u32) {
        (self.pixel_width, self.pixel_height)
    }

    /// The same mode, whatever its id.
    pub fn same(&self, other: &Mode) -> bool {
        (
            self.width,
            self.height,
            self.pixel_width,
            self.pixel_height,
            self.refresh_mhz,
        ) == (
            other.width,
            other.height,
            other.pixel_width,
            other.pixel_height,
            other.refresh_mhz,
        )
    }

    fn area(&self) -> u64 {
        self.pixel_width as u64 * self.pixel_height as u64
    }

    fn fits(&self, view: (u32, u32)) -> bool {
        self.pixel_width <= view.0 && self.pixel_height <= view.1
    }

    /// How far its aspect ratio is from the window's, in steps of about 1 %,
    /// so modes that differ by rounding count as equal.
    fn aspect_steps(&self, view: (u32, u32)) -> u32 {
        let mode = self.pixel_width.max(1) as f64 / self.pixel_height.max(1) as f64;
        let window = view.0.max(1) as f64 / view.1.max(1) as f64;
        ((mode / window).ln().abs() * 100.0).round() as u32
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{} pt", self.width, self.height)?;
        if self.hidpi() {
            write!(f, " (HiDPI, {}x{} px)", self.pixel_width, self.pixel_height)?;
        }
        if self.refresh_mhz > 0 {
            write!(f, " @ {} Hz", (self.refresh_mhz + 500) / 1000)?;
        }
        Ok(())
    }
}

/// Of the modes that are at least this share of the largest one that fits,
/// the closest in aspect ratio wins: a slightly smaller mode that fills the
/// window better beats a bigger one with wide black bars.
const NEAR_LARGEST: f64 = 0.85;

/// The mode to show in a window of `view` pixels:
///
/// - the largest that fits the window in pixels, so the picture goes out
///   1:1 (within [`NEAR_LARGEST`], the closest aspect ratio first);
/// - HiDPI over the same pixels at 1x, where there is one;
/// - then the refresh rate closest to `refresh_mhz` (the display's own).
///
/// When the window is smaller than every mode, the smallest, closest in
/// aspect ratio (the picture is then scaled down a little). `None` without
/// modes.
pub fn best(modes: &[Mode], view: (u32, u32), refresh_mhz: u32) -> Option<Mode> {
    let refresh = |m: &Mode| m.refresh_mhz.abs_diff(refresh_mhz);
    let fitting: Vec<&Mode> = modes.iter().filter(|m| m.fits(view)).collect();
    if let Some(largest) = fitting.iter().map(|m| m.area()).max() {
        return fitting
            .into_iter()
            .filter(|m| m.area() as f64 >= largest as f64 * NEAR_LARGEST)
            .min_by_key(|m| {
                (
                    m.aspect_steps(view),
                    std::cmp::Reverse(m.area()),
                    !m.hidpi(),
                    refresh(m),
                )
            })
            .copied();
    }
    modes
        .iter()
        .min_by_key(|m| (m.area(), m.aspect_steps(view), !m.hidpi(), refresh(m)))
        .copied()
}

/// The marker's text: the display, the mode it had, the mode we set.
fn encode(display: u32, original: &Mode, set: &Mode) -> String {
    let mode = |m: &Mode| {
        format!(
            "{} {} {} {} {} {}",
            m.width, m.height, m.pixel_width, m.pixel_height, m.refresh_mhz, m.id
        )
    };
    format!("{display}\n{}\n{}\n", mode(original), mode(set))
}

fn decode(text: &str) -> Option<(u32, Mode, Mode)> {
    let mut lines = text.lines();
    let display = lines.next()?.trim().parse().ok()?;
    let mut mode = || -> Option<Mode> {
        let v: Vec<&str> = lines.next()?.split_whitespace().collect();
        let [w, h, pw, ph, hz, id] = v.as_slice() else {
            return None;
        };
        Some(Mode {
            width: w.parse().ok()?,
            height: h.parse().ok()?,
            pixel_width: pw.parse().ok()?,
            pixel_height: ph.parse().ok()?,
            refresh_mhz: hz.parse().ok()?,
            id: id.parse().ok()?,
        })
    };
    let original = mode()?;
    let set = mode()?;
    Some((display, original, set))
}

#[cfg(target_os = "macos")]
pub use mac::{Matcher, recover};

#[cfg(target_os = "macos")]
mod mac {
    use std::path::PathBuf;

    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, kCFBooleanTrue};
    use objc2_core_graphics::{
        CGBeginDisplayConfiguration, CGCancelDisplayConfiguration, CGCompleteDisplayConfiguration,
        CGConfigureDisplayWithDisplayMode, CGConfigureOption, CGDirectDisplayID,
        CGDisplayConfigRef, CGDisplayCopyAllDisplayModes, CGDisplayCopyDisplayMode, CGDisplayMode,
        CGError, kCGDisplayShowDuplicateLowResolutionModes,
    };

    use super::{Mode, best, decode, encode};

    fn mode_of(m: &CGDisplayMode) -> Mode {
        let m = Some(m);
        Mode {
            width: CGDisplayMode::width(m) as u32,
            height: CGDisplayMode::height(m) as u32,
            pixel_width: CGDisplayMode::pixel_width(m) as u32,
            pixel_height: CGDisplayMode::pixel_height(m) as u32,
            refresh_mhz: (CGDisplayMode::refresh_rate(m) * 1000.0).round() as u32,
            id: CGDisplayMode::io_display_mode_id(m),
        }
    }

    /// Every mode the display offers for the desktop, the HiDPI ones too.
    fn all_modes(display: CGDirectDisplayID) -> Vec<(Mode, CFRetained<CGDisplayMode>)> {
        let Some(yes) = (unsafe { kCFBooleanTrue }) else {
            return Vec::new();
        };
        let key: &CFString = unsafe { kCGDisplayShowDuplicateLowResolutionModes };
        let options = CFDictionary::<CFString, CFBoolean>::from_slices(&[key], &[yes]);
        let Some(list) = (unsafe { CGDisplayCopyAllDisplayModes(display, Some(options.as_ref())) })
        else {
            return Vec::new();
        };
        // The array holds display modes.
        let list = unsafe { list.cast_unchecked::<CGDisplayMode>() };
        list.to_vec()
            .into_iter()
            .filter(|m| CGDisplayMode::is_usable_for_desktop_gui(Some(m)))
            .map(|m| (mode_of(&m), m))
            .collect()
    }

    fn current(display: CGDirectDisplayID) -> Option<Mode> {
        CGDisplayCopyDisplayMode(display).map(|m| mode_of(&m))
    }

    /// Switches `display` to `mode` (one of its own, found again by id or
    /// size), for the lifetime of this process at most.
    fn set(display: CGDirectDisplayID, mode: &Mode) -> Result<(), String> {
        let modes = all_modes(display);
        let found = modes
            .iter()
            .find(|(m, _)| m.id == mode.id && m.same(mode))
            .or_else(|| modes.iter().find(|(m, _)| m.same(mode)))
            .ok_or_else(|| format!("display {display} no longer offers {mode}"))?;
        let check = |e: CGError, what: &str| {
            if e == CGError::Success {
                Ok(())
            } else {
                Err(format!("{what}: CGError {}", e.0))
            }
        };
        let mut config: CGDisplayConfigRef = std::ptr::null_mut();
        unsafe {
            check(
                CGBeginDisplayConfiguration(&mut config),
                "beginning configuration",
            )?;
            if let Err(e) = check(
                CGConfigureDisplayWithDisplayMode(config, display, Some(&found.1), None),
                "configuring the mode",
            ) {
                CGCancelDisplayConfiguration(config);
                return Err(e);
            }
            // Reverted by the window server when this process ends.
            check(
                CGCompleteDisplayConfiguration(config, CGConfigureOption::ForAppOnly),
                "switching the mode",
            )
        }
    }

    /// Waits (briefly) until the display reports `mode`, so a capture
    /// started next sees the new size.
    fn settle(display: CGDirectDisplayID, mode: &Mode) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !current(display).is_some_and(|m| m.same(mode))
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Where we note the mode to go back to: per user, like the mute marker.
    fn marker() -> PathBuf {
        std::env::temp_dir().join("aetherdesk-display")
    }

    /// The display under `match`. Puts the original mode back when dropped.
    pub struct Matcher {
        display: CGDirectDisplayID,
        /// The mode before we changed it, while we have.
        original: Option<Mode>,
    }

    impl Matcher {
        pub fn new(display: CGDirectDisplayID) -> Self {
            Self {
                display,
                original: None,
            }
        }

        /// Whether the display is in a mode we set.
        pub fn changed(&self) -> bool {
            self.original.is_some()
        }

        /// The best mode for a window of `view` pixels, if the display
        /// isn't in it already.
        pub fn plan(&self, view: (u32, u32)) -> Result<Option<Mode>, String> {
            let now = current(self.display).ok_or("no current display mode")?;
            let refresh = self.original.unwrap_or(now).refresh_mhz;
            let modes: Vec<Mode> = all_modes(self.display).into_iter().map(|m| m.0).collect();
            let wanted =
                best(&modes, view, refresh).ok_or("the display lists no modes for the desktop")?;
            Ok((!wanted.same(&now)).then_some(wanted))
        }

        /// Switches the display to `wanted` (from [`plan`](Self::plan)).
        pub fn switch(&mut self, wanted: &Mode) -> Result<(), String> {
            let now = current(self.display).ok_or("no current display mode")?;
            let original = self.original.unwrap_or(now);
            if wanted.same(&original) {
                self.restore();
                return Ok(());
            }
            // Noted first: if we die after switching, the next agent knows.
            let _ = std::fs::write(marker(), encode(self.display, &original, wanted));
            if let Err(e) = set(self.display, wanted) {
                // Still in the mode it was in.
                let _ = match self.original {
                    None => std::fs::remove_file(marker()),
                    Some(_) => std::fs::write(marker(), encode(self.display, &original, &now)),
                };
                return Err(e);
            }
            self.original = Some(original);
            settle(self.display, wanted);
            println!("display: {now} → {wanted} (match)");
            Ok(())
        }

        /// Back to the mode the display had. True when it changed.
        pub fn restore(&mut self) -> bool {
            let Some(original) = self.original.take() else {
                return false;
            };
            let _ = std::fs::remove_file(marker());
            match set(self.display, &original) {
                Ok(()) => {
                    settle(self.display, &original);
                    println!("display: back to {original}");
                    true
                }
                Err(e) => {
                    eprintln!("display: can't go back to {original}: {e}");
                    false
                }
            }
        }
    }

    impl Drop for Matcher {
        fn drop(&mut self) {
            self.restore();
        }
    }

    /// Puts back the mode a previous run left changed (it crashed or was
    /// killed mid-session), unless someone changed it since. Call at start
    /// and on the way out.
    pub fn recover() {
        let path = marker();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let _ = std::fs::remove_file(&path);
        let Some((display, original, ours)) = decode(&text) else {
            return;
        };
        match current(display) {
            Some(now) if now.same(&ours) => {
                println!("display: a previous run left {now}; back to {original}");
                if let Err(e) = set(display, &original) {
                    eprintln!("display: can't go back to {original}: {e}");
                }
            }
            // The window server reverted it already (or someone picked
            // another mode, or the display is gone): leave it.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(w: u32, h: u32, scale: u32, hz: u32) -> Mode {
        Mode {
            width: w,
            height: h,
            pixel_width: w * scale,
            pixel_height: h * scale,
            refresh_mhz: hz * 1000,
            id: (w * 10 + scale) as i32,
        }
    }

    /// Roughly what a 14" MacBook Pro (3024×1964, notch) lists with
    /// duplicate low-resolution modes shown.
    fn macbook() -> Vec<Mode> {
        let mut modes = Vec::new();
        for (w, h) in [
            (1800, 1169),
            (1512, 982),
            (1352, 878),
            (1147, 745),
            (1024, 665),
            (800, 520),
        ] {
            modes.push(mode(w, h, 2, 120));
            modes.push(mode(w, h, 1, 120));
        }
        modes.push(mode(3024, 1964, 1, 120));
        modes.push(mode(1920, 1200, 1, 120));
        modes.push(mode(1440, 900, 1, 120));
        modes
    }

    #[test]
    fn largest_mode_that_fits_hidpi_first() {
        // A HiDPI Linux window of 1255×674 points.
        let m = best(&macbook(), (2510, 1348), 120_000).unwrap();
        assert_eq!((m.width, m.height, m.hidpi()), (1024, 665, true));
        // The window from #33, on a 1x screen: no HiDPI mode is that small.
        let m = best(&macbook(), (1255, 674), 120_000).unwrap();
        assert_eq!((m.pixels(), m.hidpi()), ((1024, 665), false));
        // Full screen on a 4K monitor.
        let m = best(&macbook(), (3840, 2160), 120_000).unwrap();
        assert_eq!(m.pixels(), (3024, 1964));
        assert!(m.fits((3840, 2160)));
    }

    #[test]
    fn aspect_ratio_decides_among_nearly_as_large() {
        // A 16:10 mode and a slightly larger one in the notch's aspect:
        // a 16:10 window takes the 16:10 one.
        let modes = [mode(1920, 1200, 1, 60), mode(1960, 1272, 1, 60)];
        let m = best(&modes, (2048, 1280), 60_000).unwrap();
        assert_eq!(m.pixels(), (1920, 1200));
        // But not a much smaller one just for its aspect.
        let modes = [mode(1280, 800, 1, 60), mode(1960, 1272, 1, 60)];
        assert_eq!(
            best(&modes, (2048, 1280), 60_000).unwrap().pixels(),
            (1960, 1272)
        );
    }

    #[test]
    fn refresh_rate_breaks_ties_toward_the_displays_own() {
        let modes = [
            mode(1920, 1080, 1, 50),
            mode(1920, 1080, 1, 60),
            mode(1920, 1080, 1, 30),
        ];
        let m = best(&modes, (2560, 1440), 60_000).unwrap();
        assert_eq!(m.refresh_mhz, 60_000);
        assert_eq!(
            best(&modes, (2560, 1440), 50_000).unwrap().refresh_mhz,
            50_000
        );
    }

    #[test]
    fn a_tiny_window_gets_the_smallest_mode() {
        let m = best(&macbook(), (600, 400), 120_000).unwrap();
        assert_eq!(m.pixels(), (800, 520));
        assert!(best(&[], (600, 400), 0).is_none());
    }

    #[test]
    fn marker_round_trips_and_ignores_junk() {
        let (a, b) = (mode(1512, 982, 2, 120), mode(1024, 665, 1, 60));
        assert_eq!(decode(&encode(4, &a, &b)), Some((4, a, b)));
        assert_eq!(decode("garbage"), None);
        assert_eq!(decode("1\n1 2 3\n"), None);
    }

    #[test]
    fn modes_say_what_they_are() {
        assert_eq!(
            mode(1512, 982, 2, 120).to_string(),
            "1512x982 pt (HiDPI, 3024x1964 px) @ 120 Hz"
        );
        assert_eq!(mode(1920, 1080, 1, 0).to_string(), "1920x1080 pt");
    }
}
