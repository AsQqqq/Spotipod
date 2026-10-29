//! The official Spotify app on this machine: launching it and recognising its Connect device.

use std::process::{Command, Stdio};

use crate::error::{CoreError, Result};

/// Starts the Spotify app in the background (no-op if it is already running).
pub fn launch() -> Result<()> {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.args(["-g", "-a", "Spotify"]);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", "spotify:"]);
        c
    } else {
        Command::new("spotify")
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| CoreError::Playback(format!("cannot start Spotify: {e}")))?;
    Ok(())
}

/// Name the desktop app uses for its Connect device: the computer name.
pub fn computer_name() -> Option<String> {
    let out = if cfg!(target_os = "macos") {
        Command::new("scutil")
            .args(["--get", "ComputerName"])
            .output()
    } else if cfg!(target_os = "windows") {
        return std::env::var("COMPUTERNAME").ok();
    } else {
        Command::new("hostname").output()
    }
    .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!name.is_empty()).then_some(name)
}
