// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

use std::collections::HashMap;

use base64::Engine;
use livi_core_proto::state::{NowPlaying, Protocol, Sessions};
use livi_ui_model::media::{Dab, dab};
use serde_json::Value;
use slint::{Rgba8Pixel, SharedPixelBuffer};

const EMPTY: &str = "—";

pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;

pub struct MediaView {
    pub phone_connected: bool,
    pub has_metadata: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub app: String,
    pub duration_ms: f64,
    pub elapsed_ms: Option<f64>,
    pub playing: bool,
    pub can_seek: bool,
    pub artwork: Option<Option<Pixels>>,
    pub session: String,
}

pub struct DabView {
    pub dab: Dab,
    pub logos: Vec<Option<Pixels>>,
    pub tuned_logo: Option<Pixels>,
}

#[derive(Default)]
pub struct MediaReader {
    artwork: Option<String>,
    logos: HashMap<String, Option<Pixels>>,
    last_dab: Option<Dab>,
}

fn text_or_dash(v: Option<String>) -> String {
    v.filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| EMPTY.into())
}

impl MediaReader {
    pub fn read(&mut self, state: &Value) -> MediaView {
        let mut now: NowPlaying =
            serde_json::from_value(state["nowPlaying"].clone()).unwrap_or_default();
        let sessions: Sessions =
            serde_json::from_value(state["sessions"].clone()).unwrap_or_default();
        let phone_connected = sessions.active.is_some();
        if !phone_connected {
            now = NowPlaying::default();
        }
        let has_metadata = [&now.title, &now.artist, &now.album]
            .into_iter()
            .any(|v| v.as_deref().is_some_and(|s| !s.trim().is_empty()));
        let artwork = (now.artwork != self.artwork).then(|| {
            self.artwork = now.artwork.clone();
            now.artwork.as_deref().and_then(decode)
        });
        MediaView {
            phone_connected,
            has_metadata,
            title: text_or_dash(now.title),
            artist: text_or_dash(now.artist),
            album: now.album.unwrap_or_default(),
            app: now.app.unwrap_or_default(),
            duration_ms: now.duration_ms.unwrap_or(0.0),
            elapsed_ms: now.elapsed_ms,
            playing: now.playing.unwrap_or(false),
            can_seek: sessions.active == Some(Protocol::Carplay),
            artwork,
            session: format!(
                "{}:{}",
                state["sessions"]["active"], state["sessions"]["position"]
            ),
        }
    }

    pub fn read_dab(&mut self, state: &Value) -> Option<DabView> {
        let next = dab(state);
        if self.last_dab.as_ref() == Some(&next) {
            return None;
        }
        self.last_dab = Some(next.clone());
        let mut logo = |b64: &Option<String>| {
            b64.as_ref().and_then(|s| {
                self.logos
                    .entry(s.clone())
                    .or_insert_with(|| decode(s))
                    .clone()
            })
        };
        let logos = next.stations.iter().map(|s| logo(&s.logo)).collect();
        let tuned_logo = logo(&next.tuned_logo);
        Some(DabView {
            dab: next,
            logos,
            tuned_logo,
        })
    }
}

pub fn decode(base64: &str) -> Option<Pixels> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64)
        .ok()?;
    let img = image::load_from_memory(&bytes).ok()?.into_rgba8();
    Some(SharedPixelBuffer::clone_from_slice(
        img.as_raw(),
        img.width(),
        img.height(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(protocol: Value, now: Value) -> Value {
        json!({
            "sessions": { "active": protocol, "position": 0, "total": 1 },
            "nowPlaying": now
        })
    }

    #[test]
    fn disconnected_phone_ignores_stale_media() {
        let mut reader = MediaReader::default();
        reader.read(&state(json!("carplay"), json!({ "artwork": "stale" })));
        let m = reader.read(&state(
            Value::Null,
            json!({
                "title": "Old track", "artist": "Old artist", "album": "Old album",
                "app": "Music", "playing": true, "durationMs": 60000, "elapsedMs": 20000
            }),
        ));
        assert!(!m.phone_connected && !m.has_metadata && !m.can_seek && !m.playing);
        assert_eq!(m.title, EMPTY);
        assert_eq!(m.artist, EMPTY);
        assert!(m.album.is_empty() && m.app.is_empty());
        assert_eq!(m.duration_ms, 0.0);
        assert_eq!(m.elapsed_ms, None);
        assert!(matches!(m.artwork, Some(None)));
    }

    #[test]
    fn connected_without_track_metadata_keeps_controls_available() {
        for protocol in ["carplay", "androidauto"] {
            let m = MediaReader::default().read(&state(
                json!(protocol),
                json!({
                    "title": "  ", "artist": "\t", "album": "", "app": "Music"
                }),
            ));
            assert!(m.phone_connected);
            assert!(!m.has_metadata);
            assert_eq!(m.can_seek, protocol == "carplay");
        }
    }

    #[test]
    fn partial_and_paused_metadata_are_not_empty() {
        for field in ["title", "artist", "album"] {
            let mut now = json!({ "playing": false });
            now[field] = json!("Track information");
            let m = MediaReader::default().read(&state(json!("androidauto"), now));
            assert!(m.phone_connected && m.has_metadata);
            assert!(!m.playing && !m.can_seek);
        }
    }

    #[test]
    fn metadata_can_arrive_clear_and_disconnect() {
        let mut reader = MediaReader::default();
        for (protocol, now, connected, metadata) in [
            (json!("carplay"), json!({}), true, false),
            (json!("carplay"), json!({ "title": "Song" }), true, true),
            (json!("carplay"), json!({}), true, false),
            (Value::Null, json!({}), false, false),
        ] {
            let m = reader.read(&state(protocol, now));
            assert_eq!((m.phone_connected, m.has_metadata), (connected, metadata));
        }
    }
}
