//! Flipper → PC commands.
//!
//! The Flipper sends a 3-byte frame: `[0xC1, action, value]`. Anything that
//! does not match is ignored, so a stray BLE notification can never trigger
//! an action.
//!
//! Media keys are synthesised rather than driven through COM: the OS already
//! routes VK_VOLUME_* and VK_MEDIA_* to whatever app owns the mixer or the
//! media session, which keeps this module small and dependency-light.

pub const CMD_MAGIC: u8 = 0xC1;

const VK_VOLUME_MUTE: u16 = 0xAD;
const VK_VOLUME_DOWN: u16 = 0xAE;
const VK_VOLUME_UP: u16 = 0xAF;
const VK_MEDIA_NEXT_TRACK: u16 = 0xB0;
const VK_MEDIA_PREV_TRACK: u16 = 0xB1;
const VK_MEDIA_PLAY_PAUSE: u16 = 0xB3;

pub fn handle(bytes: &[u8]) {
    if bytes.len() < 2 || bytes[0] != CMD_MAGIC {
        return;
    }

    println!("Command received: {:#04x}", bytes[1]);

    match bytes[1] {
        0x01 => tap_key(VK_VOLUME_UP),
        0x02 => tap_key(VK_VOLUME_DOWN),
        0x03 => tap_key(VK_VOLUME_MUTE),
        0x04 => tap_key(VK_MEDIA_PLAY_PAUSE),
        0x05 => tap_key(VK_MEDIA_NEXT_TRACK),
        0x06 => tap_key(VK_MEDIA_PREV_TRACK),
        0x07 => lock_workstation(),
        other => println!("Unknown command: {:#04x}", other),
    }
}

#[cfg(target_os = "windows")]
fn tap_key(vk: u16) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
    };

    unsafe {
        keybd_event(vk as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(vk as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

#[cfg(not(target_os = "windows"))]
fn tap_key(_vk: u16) {}

fn lock_workstation() {
    #[cfg(target_os = "windows")]
    {
        use std::process::Command;

        let _ = Command::new("rundll32.exe")
            .args(["user32.dll,LockWorkStation"])
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_wrong_magic_and_short_frames() {
        // These must never panic or act.
        handle(&[]);
        handle(&[0x00]);
        handle(&[CMD_MAGIC]);
        handle(&[0x00, 0x01]);
    }
}
