// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

pub mod mgmt;
pub mod sdp;

#[cfg(target_os = "linux")]
mod server;
#[cfg(target_os = "linux")]
pub use server::{Config, NameSource, run};
