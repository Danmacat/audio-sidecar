//! Linux capture backend built on the PulseAudio API. On PipeWire systems the
//! same API is provided by pipewire-pulse.

pub mod devices;
pub(crate) mod pulse;

#[cfg(test)]
mod probe;
