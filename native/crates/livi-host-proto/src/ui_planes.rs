// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

pub const CREATE: u8 = 1;
pub const FRAME: u8 = 2;
pub const FLUSH: u8 = 3;
pub const PLAY_URL: u8 = 4;
pub const PLAY_RATE: u8 = 5;
pub const PLAY_SEEK: u8 = 6;
pub const PLAY_MUTE: u8 = 7;
pub const STATUS: u8 = 8;
pub const LOG: u8 = 9;
pub const CAMERA: u8 = 10;

pub const PATH_ENV: &str = "LIVI_UI_PLANES";

// Native operations never reuse CREATE/FRAME payloads or append to codec_data.
pub const NATIVE_CAPABILITIES: u8 = 64;
pub const NATIVE_CREATE: u8 = 65;
pub const NATIVE_FRAME: u8 = 66;
pub const NATIVE_STOP: u8 = 67;
pub const NATIVE_FLUSH: u8 = 68;
pub const NATIVE_ACTIVE: u8 = 70;
pub const NATIVE_CAPABILITIES_REPLY: u8 = 64;
pub const NATIVE_FEEDBACK: u8 = 65;

/// Validate direction/opcode as well as the typed payload. Decoding a packet
/// does not negotiate a capability or authorize a renderer.
pub fn parse_native(op: u8, reply: bool, body: &[u8]) -> Option<crate::native::Packet> {
    use crate::native::Packet;
    let packet = Packet::decode(body)?;
    let matches = match (&packet, reply) {
        (Packet::Capabilities(_), _) => op == NATIVE_CAPABILITIES,
        (Packet::Create(_), false) => op == NATIVE_CREATE,
        (Packet::Data { .. }, false) => op == NATIVE_FRAME,
        (Packet::Stop(_), false) => op == NATIVE_STOP,
        (Packet::Flush(_), false) => op == NATIVE_FLUSH,
        (Packet::Active { .. }, false) => op == NATIVE_ACTIVE,
        (Packet::Feedback(_), true) => op == NATIVE_FEEDBACK,
        _ => false,
    };
    matches.then_some(packet)
}
