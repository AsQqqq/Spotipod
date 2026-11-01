//! System audio output. The Spotify app plays through the system default output, so
//! switching it moves playback live.

use crate::error::{CoreError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDevice {
    /// Stable id for `set_default`.
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// Output devices, default first.
pub fn list() -> Result<Vec<OutputDevice>> {
    let mut devices = imp::list()?;
    devices.sort_by_key(|d| !d.is_default);
    Ok(devices)
}

/// Makes `id` (from `list`) the system default output.
pub fn set_default(id: &str) -> Result<()> {
    imp::set_default(id)
}

#[cfg(target_os = "macos")]
mod imp {
    use std::{ffi::c_void, mem::size_of, ptr::NonNull};

    use objc2_core_audio::{
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID as AudioDeviceID,
        AudioObjectPropertyAddress, AudioObjectPropertyScope, AudioObjectPropertySelector,
        AudioObjectSetPropertyData, kAudioDevicePropertyStreams,
        kAudioHardwarePropertyDefaultOutputDevice, kAudioHardwarePropertyDevices,
        kAudioObjectPropertyElementMain, kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyScopeOutput, kAudioObjectSystemObject,
    };
    use objc2_core_foundation::{CFRetained, CFString};

    use super::{CoreError, OutputDevice, Result};

    const SYSTEM: u32 = kAudioObjectSystemObject as u32;

    fn address(
        selector: AudioObjectPropertySelector,
        scope: AudioObjectPropertyScope,
    ) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn check(status: i32, what: &str) -> Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(CoreError::Audio(format!(
                "{what} failed (OSStatus {status})"
            )))
        }
    }

    fn data_size(object: u32, addr: &AudioObjectPropertyAddress) -> Result<u32> {
        let mut size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                object,
                NonNull::from(addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
            )
        };
        check(status, "AudioObjectGetPropertyDataSize")?;
        Ok(size)
    }

    fn get<T: Copy>(object: u32, addr: &AudioObjectPropertyAddress, mut value: T) -> Result<T> {
        let mut size = size_of::<T>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast::<c_void>(),
            )
        };
        check(status, "AudioObjectGetPropertyData")?;
        Ok(value)
    }

    fn device_ids() -> Result<Vec<AudioDeviceID>> {
        let addr = address(
            kAudioHardwarePropertyDevices,
            kAudioObjectPropertyScopeGlobal,
        );
        let mut size = data_size(SYSTEM, &addr)?;
        let mut ids = vec![0 as AudioDeviceID; size as usize / size_of::<AudioDeviceID>()];
        if ids.is_empty() {
            return Ok(ids);
        }
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                NonNull::from(&mut size),
                NonNull::new_unchecked(ids.as_mut_ptr()).cast::<c_void>(),
            )
        };
        check(status, "list audio devices")?;
        ids.truncate(size as usize / size_of::<AudioDeviceID>());
        Ok(ids)
    }

    fn has_output(id: AudioDeviceID) -> bool {
        let addr = address(kAudioDevicePropertyStreams, kAudioObjectPropertyScopeOutput);
        data_size(id, &addr).is_ok_and(|s| s > 0)
    }

    fn name(id: AudioDeviceID) -> Option<String> {
        let addr = address(kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal);
        let raw: *const CFString = get(id, &addr, std::ptr::null()).ok()?;
        // The property returns a retained CFString.
        let cf = unsafe { CFRetained::from_raw(NonNull::new(raw as *mut CFString)?) };
        Some(cf.to_string())
    }

    fn default_output() -> Option<AudioDeviceID> {
        let addr = address(
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
        );
        get(SYSTEM, &addr, 0 as AudioDeviceID).ok()
    }

    pub fn list() -> Result<Vec<OutputDevice>> {
        let default = default_output();
        Ok(device_ids()?
            .into_iter()
            .filter(|&id| has_output(id))
            .filter_map(|id| {
                Some(OutputDevice {
                    id: id.to_string(),
                    name: name(id)?,
                    is_default: Some(id) == default,
                })
            })
            .collect())
    }

    pub fn set_default(id: &str) -> Result<()> {
        let mut device: AudioDeviceID = id
            .parse()
            .map_err(|_| CoreError::InvalidArgument(format!("bad output id {id}")))?;
        let addr = address(
            kAudioHardwarePropertyDefaultOutputDevice,
            kAudioObjectPropertyScopeGlobal,
        );
        let status = unsafe {
            AudioObjectSetPropertyData(
                SYSTEM,
                NonNull::from(&addr),
                0,
                std::ptr::null(),
                size_of::<AudioDeviceID>() as u32,
                NonNull::from(&mut device).cast::<c_void>(),
            )
        };
        check(status, "set default output")
    }
}

/// PulseAudio / PipeWire via `pactl`.
#[cfg(target_os = "linux")]
mod imp {
    use std::process::Command;

    use super::{CoreError, OutputDevice, Result};

    fn pactl(args: &[&str]) -> Result<String> {
        let out = Command::new("pactl")
            .args(args)
            .output()
            .map_err(|e| CoreError::Audio(format!("pactl: {e}")))?;
        if !out.status.success() {
            return Err(CoreError::Audio(format!(
                "pactl {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub fn list() -> Result<Vec<OutputDevice>> {
        let default = pactl(&["get-default-sink"])?.trim().to_owned();
        let mut devices = Vec::new();
        let mut id = None;
        for line in pactl(&["list", "sinks"])?.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("Name: ") {
                id = Some(v.to_owned());
            } else if let (Some(v), Some(sink)) = (line.strip_prefix("Description: "), id.take()) {
                devices.push(OutputDevice {
                    is_default: sink == default,
                    id: sink,
                    name: v.to_owned(),
                });
            }
        }
        Ok(devices)
    }

    pub fn set_default(id: &str) -> Result<()> {
        pactl(&["set-default-sink", id]).map(|_| ())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{CoreError, OutputDevice, Result};

    pub fn list() -> Result<Vec<OutputDevice>> {
        Err(CoreError::Audio(
            "output switching is not supported on this OS yet".into(),
        ))
    }

    pub fn set_default(_id: &str) -> Result<()> {
        list().map(|_| ())
    }
}
