// SPDX-License-Identifier: GPL-3.0-or-later

//! Private, unprivileged native Bluetooth bootstrap transport. No hardware,
//! TCP listener, shared configuration, session pipeline, or application loop.
#![cfg(target_os = "macos")]

pub mod config;
pub mod ipc;
pub mod router;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Unauthorized,
    UnsafeEndpoint,
    Occupied,
    Disconnected,
    Malformed,
    Descriptor,
    Backpressure,
    Stale,
    TimedOut,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "private Bluetooth handoff: {self:?}")
    }
}

impl std::error::Error for Error {}
