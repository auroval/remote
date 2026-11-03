//! Points the Mac's output at the AetherDesk device for a session (#82), so
//! what apps play goes to the client and not to the Mac's speakers. Both
//! the default output and the one for alerts follow; at the end they go
//! back to what they were, unless the user picked something else meanwhile.
//! A marker file covers a crash in between: the next start finds it and
//! switches back ([`recover`]), as [`crate::mute`] does for muting.

use std::path::PathBuf;
use std::ptr::NonNull;

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertySelector, AudioObjectSetPropertyData, kAudioDevicePropertyDeviceUID,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDefaultSystemOutputDevice,
    kAudioHardwarePropertyTranslateUIDToDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};
use objc2_core_foundation::{CFRetained, CFString};

/// The device's UID, as the plug-in (driver/driver.c) reports it.
pub const DEVICE_UID: &str = "nl.stormlantern.AetherDesk.speaker";

/// The outputs we switch: the default one, and the one for alerts.
const OUTPUTS: [AudioObjectPropertySelector; 2] = [
    kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDefaultSystemOutputDevice,
];

fn address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

fn system() -> AudioObjectID {
    kAudioObjectSystemObject as AudioObjectID
}

fn current(output: AudioObjectPropertySelector) -> Option<AudioObjectID> {
    let addr = address(output);
    let mut device: AudioObjectID = 0;
    let mut size = size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            system(),
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast(),
        )
    };
    (status == 0 && device != 0).then_some(device)
}

fn set(output: AudioObjectPropertySelector, device: AudioObjectID) -> bool {
    let addr = address(output);
    let status = unsafe {
        AudioObjectSetPropertyData(
            system(),
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            size_of::<AudioObjectID>() as u32,
            NonNull::from(&device).cast(),
        )
    };
    status == 0
}

/// The device with this UID, if it's there.
fn device(uid: &str) -> Option<AudioObjectID> {
    let uid = CFString::from_str(uid);
    let qualifier: *const CFString = &*uid;
    let addr = address(kAudioHardwarePropertyTranslateUIDToDevice);
    let mut device: AudioObjectID = 0;
    let mut size = size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            system(),
            NonNull::from(&addr),
            size_of::<*const CFString>() as u32,
            (&raw const qualifier).cast(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast(),
        )
    };
    (status == 0 && device != 0).then_some(device)
}

/// A device's UID: IDs change when coreaudiod restarts, UIDs don't.
fn uid(device: AudioObjectID) -> Option<String> {
    let addr = address(kAudioDevicePropertyDeviceUID);
    let mut value: *const CFString = std::ptr::null();
    let mut size = size_of::<*const CFString>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&addr),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    let value = NonNull::new(value.cast_mut()).filter(|_| status == 0)?;
    // The caller owns the string it gets back.
    let value: CFRetained<CFString> = unsafe { CFRetained::from_raw(value) };
    Some(value.to_string())
}

/// Where we note what we switched: per user, like the mute marker.
fn marker() -> PathBuf {
    std::env::temp_dir().join("aetherdesk-output")
}

/// The Mac's outputs, on AetherDesk until this is dropped.
pub struct Switched {
    /// Each output we switched, and the UID of the device it had.
    previous: Vec<(AudioObjectPropertySelector, String)>,
}

/// Makes AetherDesk the Mac's output, or says why not.
pub fn switch() -> Result<Switched, String> {
    let ours = device(DEVICE_UID).ok_or("no AetherDesk device (plug-in not installed?)")?;
    let mut switched = Switched {
        previous: Vec::new(),
    };
    for output in OUTPUTS {
        let Some(was) = current(output) else {
            continue;
        };
        if was == ours {
            continue; // nothing to put back
        }
        let Some(was_uid) = uid(was) else {
            continue;
        };
        if !set(output, ours) {
            // `switched` drops here and puts back what it changed so far.
            return Err("switching the Mac's output to AetherDesk failed".into());
        }
        switched.previous.push((output, was_uid));
    }
    let _ = std::fs::write(marker(), encode(&switched.previous));
    println!("sound: the Mac plays to AetherDesk for the session");
    Ok(switched)
}

fn encode(previous: &[(AudioObjectPropertySelector, String)]) -> String {
    previous
        .iter()
        .map(|(output, uid)| format!("{output} {uid}\n"))
        .collect()
}

fn decode(text: &str) -> Vec<(AudioObjectPropertySelector, String)> {
    text.lines()
        .filter_map(|l| {
            let (output, uid) = l.split_once(' ')?;
            let output = output.parse().ok()?;
            OUTPUTS.contains(&output).then(|| (output, uid.to_string()))
        })
        .collect()
}

/// Puts each output back, unless the user changed it meanwhile.
fn restore(previous: &[(AudioObjectPropertySelector, String)]) {
    let ours = device(DEVICE_UID);
    let mut restored = 0;
    for (output, was) in previous {
        if ours.is_some() && current(*output) != ours {
            continue; // moved on by hand
        }
        match device(was) {
            Some(d) if set(*output, d) => restored += 1,
            // Unplugged meanwhile: macOS picks another when ours goes.
            _ => println!("sound: couldn't switch back to {was} (gone?)"),
        }
    }
    if restored > 0 {
        println!("sound: the Mac plays to its own output again");
    }
}

impl Drop for Switched {
    fn drop(&mut self) {
        restore(&self.previous);
        let _ = std::fs::remove_file(marker());
    }
}

/// Switches back what a previous run left on AetherDesk (it crashed or was
/// killed mid-session). Call once at start.
pub fn recover() {
    let path = marker();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let previous = decode(&text);
    if !previous.is_empty() {
        println!("sound: a previous run left the Mac on AetherDesk");
        restore(&previous);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_round_trips_and_ignores_junk() {
        let previous = vec![
            (OUTPUTS[0], "BuiltInSpeakerDevice".to_string()),
            (OUTPUTS[1], "a uid with spaces".to_string()),
        ];
        assert_eq!(decode(&encode(&previous)), previous);
        assert!(decode("garbage\n12 x\n").is_empty());
    }
}
