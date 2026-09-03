//! System media session backends (SMTC on Windows; MPRIS on Linux;
//! MediaRemote via the mediaremote-adapter helper on macOS).

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod windows;
