//! Files dropped on the window, for the Mac: they land where they were
//! dropped there (docs/PRD-file-transfer.md, F10).
//!
//! SDL tells about a drag over the window with its drop events: a begin,
//! positions while it moves, a file per path when it's let go, and a
//! complete (also when it leaves without a drop). The `sdl3` crate leaves
//! out the positions, so [`poll`] takes SDL's events itself and keeps the
//! drop events here.
//!
//! On Wayland SDL asks the FileTransfer portal for the files when the drag
//! offers `application/vnd.portal.filetransfer` (Dolphin does), so in the
//! Flatpak the paths are ones the sandbox may read. Not for folders: the
//! portal gives SDL nothing then, and it falls back to the host's paths
//! (see [`crate::copies`] for what the user hears).

use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::path::PathBuf;

use sdl3::EventPump;
use sdl3::event::Event;
use sdl3::sys::events::{
    SDL_EVENT_DROP_BEGIN, SDL_EVENT_DROP_COMPLETE, SDL_EVENT_DROP_FILE, SDL_EVENT_DROP_POSITION,
    SDL_Event, SDL_EventType, SDL_PollEvent,
};

/// What a drop event means for the Mac.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Files are being dragged over the window, at this point (window
    /// coordinates).
    Over(f32, f32),
    /// Files were dropped at this point (window coordinates).
    Dropped { paths: Vec<PathBuf>, at: (f32, f32) },
}

/// One of SDL's drop events, as far as we need it.
#[derive(Debug, Clone, PartialEq)]
pub enum Raw {
    Begin,
    Position(f32, f32),
    File(String, f32, f32),
    Complete,
    /// Dropped text: not files, nothing for the Mac.
    Other,
}

/// Collects one drag's files.
#[derive(Debug, Default)]
pub struct Drops {
    files: Vec<PathBuf>,
    at: Option<(f32, f32)>,
}

impl Drops {
    pub fn on(&mut self, raw: Raw) -> Option<Step> {
        match raw {
            Raw::Begin => {
                *self = Self::default();
                None
            }
            Raw::Position(x, y) => {
                self.at = Some((x, y));
                Some(Step::Over(x, y))
            }
            Raw::File(path, x, y) => {
                self.files.push(PathBuf::from(path));
                self.at.get_or_insert((x, y));
                None
            }
            Raw::Complete => {
                let done = std::mem::take(self);
                match (done.files.is_empty(), done.at) {
                    (false, Some(at)) => Some(Step::Dropped {
                        paths: done.files,
                        at,
                    }),
                    _ => None,
                }
            }
            Raw::Other => None,
        }
    }
}

/// SDL's pending events, like [`EventPump::poll_iter`], but with the drop
/// events read into `drops`; what they mean comes back in `steps`.
pub fn poll(_pump: &mut EventPump, drops: &mut Drops, steps: &mut Vec<Step>) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let mut raw = MaybeUninit::<SDL_Event>::uninit();
        // SAFETY: SDL fills the event when it says there is one; the pump
        // shows we're on SDL's thread.
        if !unsafe { SDL_PollEvent(raw.as_mut_ptr()) } {
            break;
        }
        let raw = unsafe { raw.assume_init() };
        match drop_event(&raw) {
            Some(r) => steps.extend(drops.on(r)),
            None => events.push(Event::from_ll(raw)),
        }
    }
    events
}

/// The drop event in `raw`, if it is one.
fn drop_event(raw: &SDL_Event) -> Option<Raw> {
    // SAFETY: every event starts with its type.
    let kind = SDL_EventType(unsafe { raw.r#type });
    if !(SDL_EVENT_DROP_FILE.0..=SDL_EVENT_DROP_POSITION.0).contains(&kind.0) {
        return None;
    }
    // SAFETY: a drop event, by its type.
    let d = unsafe { raw.drop };
    Some(match kind {
        SDL_EVENT_DROP_BEGIN => Raw::Begin,
        SDL_EVENT_DROP_POSITION => Raw::Position(d.x, d.y),
        SDL_EVENT_DROP_COMPLETE => Raw::Complete,
        SDL_EVENT_DROP_FILE if !d.data.is_null() => {
            // SAFETY: SDL's string, valid until the next poll.
            let path = unsafe { CStr::from_ptr(d.data) };
            Raw::File(path.to_string_lossy().into_owned(), d.x, d.y)
        }
        _ => Raw::Other,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drop_gives_its_files_and_where() {
        let mut d = Drops::default();
        assert_eq!(d.on(Raw::Begin), None);
        assert_eq!(
            d.on(Raw::Position(10.0, 20.0)),
            Some(Step::Over(10.0, 20.0))
        );
        assert_eq!(
            d.on(Raw::Position(30.0, 40.0)),
            Some(Step::Over(30.0, 40.0))
        );
        assert_eq!(d.on(Raw::File("/a".into(), 30.0, 40.0)), None);
        assert_eq!(d.on(Raw::File("/b c".into(), 30.0, 40.0)), None);
        assert_eq!(
            d.on(Raw::Complete),
            Some(Step::Dropped {
                paths: vec![PathBuf::from("/a"), PathBuf::from("/b c")],
                at: (30.0, 40.0)
            })
        );
        // And the next drag starts empty.
        assert_eq!(d.on(Raw::Complete), None);
    }

    #[test]
    fn a_drag_that_leaves_or_drops_text_drops_nothing() {
        let mut d = Drops::default();
        d.on(Raw::Begin);
        d.on(Raw::Position(1.0, 2.0));
        assert_eq!(d.on(Raw::Complete), None);
        d.on(Raw::Begin);
        d.on(Raw::Position(1.0, 2.0));
        assert_eq!(d.on(Raw::Other), None);
        assert_eq!(d.on(Raw::Complete), None);
    }

    #[test]
    fn a_file_without_a_position_has_its_own() {
        let mut d = Drops::default();
        d.on(Raw::Begin);
        d.on(Raw::File("/a".into(), 5.0, 6.0));
        assert_eq!(
            d.on(Raw::Complete),
            Some(Step::Dropped {
                paths: vec![PathBuf::from("/a")],
                at: (5.0, 6.0)
            })
        );
    }

    #[test]
    fn only_drop_events_are_taken() {
        let mut raw: SDL_Event = unsafe { std::mem::zeroed() };
        raw.r#type = SDL_EVENT_DROP_POSITION.0;
        raw.drop.x = 3.0;
        raw.drop.y = 4.0;
        assert_eq!(drop_event(&raw), Some(Raw::Position(3.0, 4.0)));
        raw.r#type = SDL_EVENT_DROP_FILE.0;
        assert_eq!(drop_event(&raw), Some(Raw::Other), "no path");
        raw.r#type = sdl3::sys::events::SDL_EVENT_MOUSE_MOTION.0;
        assert_eq!(drop_event(&raw), None);
    }
}
