// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Experimental endpoint only. No production route constructs this decoder.
//! A decoded sample is NOT a presentation acknowledgement.

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as app;
use gstreamer_video as video;
use gstreamer_video::VideoFrameExt;

const MAX_BYTES: usize = 64 * 1024 * 1024;

/// Seconds on the macOS host clock used by Metal's presentedTime.
fn host_time() -> f64 {
    #[repr(C)]
    #[derive(Default)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }
    unsafe extern "C" {
        fn mach_absolute_time() -> u64;
        fn mach_timebase_info(info: *mut Timebase) -> i32;
    }
    static SCALE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    let scale = SCALE.get_or_init(|| {
        let mut info = Timebase::default();
        if unsafe { mach_timebase_info(&mut info) } != 0 || info.denom == 0 {
            return f64::NAN;
        }
        f64::from(info.numer) / f64::from(info.denom) / 1_000_000_000.0
    });
    unsafe { mach_absolute_time() as f64 * scale }
}

pub struct BgraFrame {
    pub width: u32,
    pub height: u32,
    /// Actual decoded sample arrival, before row copying; never an ACK.
    pub decoded_ready_time: f64,
    /// Tightly packed BGRA rows, owned independently of the GStreamer buffer.
    pub bytes: Vec<u8>,
}

pub struct Decoder {
    pipeline: gst::Pipeline,
    source: app::AppSrc,
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Decoder {
    pub fn new(
        codec: &str,
        codec_data: &[u8],
        receive: impl Fn(BgraFrame) + Send + Sync + 'static,
    ) -> Option<Self> {
        super::ensure_init();
        let decoder = super::candidates(codec, std::env::var_os("LIVI_GST_SWDEC").is_some())
            .into_iter()
            .find(|name| super::usable(name))?;
        let description = description(codec, &decoder)?;
        let pipeline = gst::parse::launch(&description)
            .ok()?
            .downcast::<gst::Pipeline>()
            .ok()?;
        let source = pipeline.by_name("src")?.downcast::<app::AppSrc>().ok()?;
        if !codec_data.is_empty() && matches!(codec, "h264" | "h265") {
            source.set_caps(Some(&super::length_prefixed_caps(codec, codec_data)));
        }
        if let Some(shader) = pipeline.by_name("cal") {
            shader.set_property("fragment", super::CAL_FRAGMENT);
            shader.set_property(
                "uniforms",
                gst::Structure::builder("uniforms")
                    .field("u_gamma", 1.0f32)
                    .field("u_contrast", 1.0f32)
                    .field("u_gain_r", 1.0f32)
                    .field("u_gain_g", 1.0f32)
                    .field("u_gain_b", 1.0f32)
                    .build(),
            );
        }
        if let Some(dec) = pipeline.by_name("dec") {
            super::install_decoder_probes(&dec, &decoder, &super::to_stderr());
        }
        let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let health = failed.clone();
        pipeline.bus()?.set_sync_handler(move |_, message| {
            if matches!(message.view(), gst::MessageView::Error(_) | gst::MessageView::Eos(_)) {
                health.store(true, std::sync::atomic::Ordering::Release);
            }
            gst::BusSyncReply::Pass
        });
        super::log_bus_messages(&pipeline);
        let sink = pipeline.by_name("sink")?.downcast::<app::AppSink>().ok()?;
        sink.set_callbacks(
            app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    if let Some(frame) = copy_frame(&sample) {
                        receive(frame);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        Some(Self { pipeline, source, failed })
    }

    pub fn start(&self) -> bool {
        self.pipeline.set_state(gst::State::Playing).is_ok()
    }

    /// Enqueue only. This result says nothing about decoding or presentation.
    pub fn push(&self, bytes: &[u8]) -> bool {
        !bytes.is_empty()
            && bytes.len() <= 4 * 1024 * 1024
            && self.source.current_level_bytes().saturating_add(bytes.len() as u64) <= 8 * 1024 * 1024
            && !self.failed()
            && self
                .source
                .push_buffer(gst::Buffer::from_slice(bytes.to_vec()))
                .is_ok()
    }

    pub fn failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn flush(&self) -> bool {
        if self.failed() { return false; }
        self.pipeline.set_state(gst::State::Ready).is_ok() && self.start()
    }

    pub fn stop(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.stop();
    }
}

fn description(codec: &str, decoder: &str) -> Option<String> {
    let cal = livi_video::codec::calibration();
    let original = livi_video::codec::pipeline_desc(codec, decoder, cal, None);
    let sink = livi_video::codec::sink_chain(None);
    let prefix = original.strip_suffix(&sink)?;
    // Calibration produces GL memory; explicitly download before CPU access.
    let download = if cal.is_empty() { "" } else { "gldownload ! " };
    Some(format!(
        "{prefix}{download}videoconvert ! video/x-raw,format=BGRA \
         ! appsink name=sink sync=false max-buffers=2 drop=true"
    ))
}

fn copy_frame(sample: &gst::Sample) -> Option<BgraFrame> {
    let decoded_ready_time = host_time();
    let info = video::VideoInfo::from_caps(sample.caps()?).ok()?;
    if info.format() != video::VideoFormat::Bgra {
        return None;
    }
    let width = info.width();
    let height = info.height();
    let row = usize::try_from(width).ok()?.checked_mul(4)?;
    let size = row.checked_mul(usize::try_from(height).ok()?)?;
    if width == 0 || height == 0 || size > MAX_BYTES {
        return None;
    }
    let frame = video::VideoFrameRef::from_buffer_ref_readable(sample.buffer()?, &info).ok()?;
    let stride = usize::try_from(frame.plane_stride()[0]).ok()?;
    if stride < row {
        return None;
    }
    let plane = frame.plane_data(0).ok()?;
    let mut bytes = Vec::with_capacity(size);
    for y in 0..usize::try_from(height).ok()? {
        let offset = y.checked_mul(stride)?;
        bytes.extend_from_slice(plane.get(offset..offset.checked_add(row)?)?);
    }
    Some(BgraFrame {
        width,
        height,
        decoded_ready_time,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experimental_description_keeps_parser_decoder_and_calibration() {
        let desc = description("h264", "avdec_h264").unwrap();
        assert!(desc.contains("h264parse"));
        assert!(desc.contains("avdec_h264 name=dec"));
        assert!(desc.contains("glshader name=cal"));
        assert!(desc.contains("gldownload ! videoconvert"));
        assert!(desc.contains("max-buffers=2 drop=true"));
        assert!(!desc.contains("glimagesink"));
    }

    #[test]
    fn production_description_is_still_glimagesink() {
        let desc = livi_video::codec::pipeline_desc("h264", "avdec_h264", "", None);
        assert!(
            desc.ends_with("glimagesink name=sink sync=false qos=false force-aspect-ratio=false")
        );
        assert!(!desc.contains("appsink"));
    }
}
