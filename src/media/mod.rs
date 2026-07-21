//! System media session backends (SMTC on Windows; MPRIS planned for Linux,
//! MediaRemote-adapter planned for macOS).

#[cfg(windows)]
#[allow(unsafe_code)]
pub mod windows;
