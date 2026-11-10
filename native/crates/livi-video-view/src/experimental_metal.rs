// SPDX-FileCopyrightText: 2025-2026 Lasse Heitgres
// SPDX-License-Identifier: GPL-3.0-or-later
// Additional terms: see NOTICE

//! Isolated macOS prototype; no production route calls this API.
//! Only a Metal drawable-presented callback can produce `Presentation`.
//! Offline tests simulate that callback and do not validate the compositor.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use livi_video_player::experimental_decode::Decoder;

const LIMIT: usize = 3;
static NEXT_PLAYER: AtomicU64 = AtomicU64::new(1);
static NEXT_FRAME: AtomicU64 = AtomicU64::new(1);

fn allocate_epoch(counter: &AtomicU64) -> Option<u64> {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let next = current.checked_add(1)?;
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return Some(current),
            Err(actual) => current = actual,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameIdentity {
    pub session_epoch: u64,
    pub player_epoch: u64,
    pub visibility_epoch: u64,
    pub frame_id: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Presentation {
    pub identity: FrameIdentity,
    /// OS-reported host time, not proof that a human saw the pixels.
    pub presented_time: f64,
    /// Actual appsink decoded delivery, not compressed-packet arrival.
    pub decoded_ready_time: Option<f64>,
}

/// Cumulative, saturating counters. Rejections count ACK callbacks only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statistics {
    pub issued_frames: u64,
    pub pending_evictions: u64,
    pub result_evictions: u64,
    pub rejected_callbacks: u64,
    pub accepted_presentations: u64,
    pub invalidated_pending_frames: u64,
    pub cleared_results: u64,
}

/// Bounded, read-only diagnostics for this isolated experimental player.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub visibility_epoch: u64,
    pub pending_frames: Vec<FrameIdentity>,
    pub queued_results: usize,
    pub statistics: Statistics,
}

struct Gate {
    session: u64,
    player: u64,
    visibility: u64,
    eligible: bool,
    cancelled: bool,
    pending: BTreeMap<u64, (FrameIdentity, Option<f64>)>,
    presented: VecDeque<Presentation>,
    decoded: VecDeque<(u64, f64)>,
    statistics: Statistics,
}

impl Gate {
    fn new(session: u64, player: u64) -> Self {
        Self {
            session,
            player,
            visibility: 0,
            eligible: false,
            cancelled: session == 0 || player == 0,
            pending: BTreeMap::new(),
            presented: VecDeque::new(),
            decoded: VecDeque::new(),
            statistics: Statistics::default(),
        }
    }

    fn visibility(&mut self, epoch: u64, eligible: bool) {
        if self.cancelled || epoch <= self.visibility {
            return;
        }
        self.visibility = epoch;
        self.eligible = eligible;
        self.clear_queues();
    }

    fn issue(&mut self) -> Option<FrameIdentity> {
        self.issue_with_counter(&NEXT_FRAME)
    }

    fn issue_decoded(&mut self, time: f64) -> Option<FrameIdentity> {
        if !time.is_finite() || time <= 0.0 {
            return None;
        }
        let identity = self.issue()?;
        self.pending.get_mut(&identity.frame_id)?.1 = Some(time);
        if self.decoded.len() == LIMIT { self.decoded.pop_front(); }
        self.decoded.push_back((identity.frame_id, time));
        Some(identity)
    }

    fn issue_with_counter(&mut self, counter: &AtomicU64) -> Option<FrameIdentity> {
        if self.cancelled || !self.eligible {
            return None;
        }
        let Some(frame_id) = allocate_epoch(counter) else {
            self.cancel();
            return None;
        };
        let identity = FrameIdentity {
            session_epoch: self.session,
            player_epoch: self.player,
            visibility_epoch: self.visibility,
            frame_id,
        };
        if self.pending.len() == LIMIT {
            self.pending.pop_first();
            self.statistics.pending_evictions = self.statistics.pending_evictions.saturating_add(1);
        }
        self.pending.insert(frame_id, (identity, None));
        self.statistics.issued_frames = self.statistics.issued_frames.saturating_add(1);
        Some(identity)
    }

    fn acknowledge(&mut self, frame: u64, visibility: u64, time: f64) {
        // Called exclusively from event 3 (drawable-presented), never from
        // decoder, enqueue, GPU completion, pre-draw, or a timer.
        if self.cancelled
            || !self.eligible
            || visibility != self.visibility
            || !time.is_finite()
            || time <= 0.0
        {
            self.reject_callback();
            return;
        }
        let Some((identity, decoded_ready_time)) = self.pending.remove(&frame) else {
            self.reject_callback();
            return;
        };
        if identity.visibility_epoch != visibility
            || decoded_ready_time.is_some_and(|decoded| time < decoded)
        {
            self.reject_callback();
            return;
        }
        if self.presented.len() == LIMIT {
            self.presented.pop_front();
            self.statistics.result_evictions = self.statistics.result_evictions.saturating_add(1);
        }
        self.statistics.accepted_presentations =
            self.statistics.accepted_presentations.saturating_add(1);
        self.presented.push_back(Presentation {
            identity,
            presented_time: time,
            decoded_ready_time,
        });
    }

    fn reject_callback(&mut self) {
        self.statistics.rejected_callbacks = self.statistics.rejected_callbacks.saturating_add(1);
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            visibility_epoch: self.visibility,
            pending_frames: self.pending.values().map(|(id, _)| *id).collect(),
            queued_results: self.presented.len(),
            statistics: self.statistics,
        }
    }

    fn clear_queues(&mut self) {
        self.statistics.invalidated_pending_frames = self
            .statistics
            .invalidated_pending_frames
            .saturating_add(self.pending.len() as u64);
        self.statistics.cleared_results = self
            .statistics
            .cleared_results
            .saturating_add(self.presented.len() as u64);
        self.pending.clear();
        self.presented.clear();
        self.decoded.clear();
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.eligible = false;
        self.clear_queues();
    }
}

type Shared = Mutex<Gate>;

unsafe extern "C" {
    fn livi_metal_prototype_create(
        parent: usize,
        context: *const c_void,
        event: extern "C" fn(*const c_void, u32, u64, u64, f64),
        release: extern "C" fn(*const c_void),
    ) -> usize;
    fn livi_metal_prototype_submit(
        handle: usize,
        bytes: *const u8,
        len: usize,
        width: u32,
        height: u32,
        frame: u64,
        visibility: u64,
    );
    fn livi_metal_prototype_visible(handle: usize, visible: bool);
    #[cfg(feature = "native-projection")]
    fn livi_metal_prototype_region(handle: usize, left: f64, top: f64, width: f64, height: f64);
    fn livi_metal_prototype_destroy(handle: usize);
}

extern "C" fn native_event(context: *const c_void, event: u32, frame: u64, epoch: u64, time: f64) {
    // The native owner retains this Arc reference until all submitted blocks
    // have released the owner. No Rust references cross the callback.
    let shared = unsafe { &*context.cast::<Shared>() };
    let mut gate = shared.lock().unwrap_or_else(|e| e.into_inner());
    match event {
        1 => gate.visibility(epoch, false),
        2 => gate.visibility(epoch, true),
        3 => gate.acknowledge(frame, epoch, time),
        4 => gate.cancel(),
        _ => {}
    }
}

extern "C" fn release_context(context: *const c_void) {
    drop(unsafe { Arc::from_raw(context.cast::<Shared>()) });
}

/// Native methods retain their owner before asynchronous work and serialize
/// all AppKit/Metal lifecycle operations on the main queue.
struct Native(usize);

impl Drop for Native {
    fn drop(&mut self) {
        unsafe { livi_metal_prototype_destroy(self.0) };
    }
}

/// Main-thread-owned experimental player. Session identity is immutable:
/// changing sessions requires cancelling/dropping this player and constructing
/// a new one. Constructing another prototype on the same parent cancels the old.
pub struct Prototype {
    decoder: Decoder,
    native: Arc<Native>,
    gate: Arc<Shared>,
    _main_thread: PhantomData<Rc<()>>,
}

impl Prototype {
    /// Creates an isolated prototype, not connected to AA or any production route.
    ///
    /// # Safety
    /// `parent` must be a valid, live LIVI AppKit view on the main thread.
    /// The caller must maintain that view until this prototype is cancelled.
    /// This explicitly initializes a decoder and Metal device when called.
    pub unsafe fn new(
        parent: usize,
        session_epoch: u64,
        codec: &str,
        codec_data: &[u8],
    ) -> Option<Self> {
        if session_epoch == 0 {
            return None;
        }
        let player = allocate_epoch(&NEXT_PLAYER)?;
        let gate = Arc::new(Mutex::new(Gate::new(session_epoch, player)));
        let context = Arc::into_raw(gate.clone()).cast::<c_void>();
        // Native create consumes context on BOTH success and failure.
        let handle =
            unsafe { livi_metal_prototype_create(parent, context, native_event, release_context) };
        if handle == 0 {
            return None;
        }
        let native = Arc::new(Native(handle));
        let output = native.clone();
        let state = gate.clone();
        let decoder = Decoder::new(codec, codec_data, move |frame| {
            let identity = state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .issue_decoded(frame.decoded_ready_time);
            if let Some(id) = identity {
                unsafe {
                    livi_metal_prototype_submit(
                        output.0,
                        frame.bytes.as_ptr(),
                        frame.bytes.len(),
                        frame.width,
                        frame.height,
                        id.frame_id,
                        id.visibility_epoch,
                    );
                }
            }
        })?;
        Some(Self {
            decoder,
            native,
            gate,
            _main_thread: PhantomData,
        })
    }

    pub fn start(&self) -> bool {
        let cancelled = self
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled;
        !cancelled && self.decoder.start()
    }

    /// Enqueue result only; never a presentation indication.
    pub fn push(&self, bytes: &[u8]) -> bool {
        let cancelled = self
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cancelled;
        !cancelled && self.decoder.push(bytes)
    }

    pub fn set_visible(&self, visible: bool) {
        unsafe { livi_metal_prototype_visible(self.native.0, visible) };
    }

    pub fn take_presentations(&self) -> Vec<Presentation> {
        self.gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .presented
            .drain(..)
            .collect()
    }

    pub fn take_decoded(&self) -> Vec<(u64, f64)> {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).decoded.drain(..).collect()
    }

    pub fn failed(&self) -> bool { self.decoder.failed() }

    pub fn flush(&self) -> bool {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).clear_queues();
        self.decoder.flush()
    }

    #[cfg(feature = "native-projection")]
    pub(crate) fn set_region(&self, r: crate::control::Region) {
        let values = [r.crop_l, r.crop_t, r.vis_w, r.vis_h, r.tier_w, r.tier_h];
        let valid = values.iter().all(|v| v.is_finite())
            && r.tier_w > 0.0 && r.tier_h > 0.0 && r.crop_l >= 0.0 && r.crop_t >= 0.0
            && r.vis_w > 0.0 && r.vis_h > 0.0
            && r.crop_l + r.vis_w <= r.tier_w && r.crop_t + r.vis_h <= r.tier_h;
        let (l, t, w, h) = if valid {
            (r.crop_l / r.tier_w, r.crop_t / r.tier_h, r.vis_w / r.tier_w, r.vis_h / r.tier_h)
        } else { (0.0, 0.0, 1.0, 1.0) };
        unsafe { livi_metal_prototype_region(self.native.0, l, t, w, h) };
    }

    pub fn snapshot(&self) -> Snapshot {
        self.gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    pub fn cancel(&self) {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).cancel();
        self.decoder.stop();
        // Hiding invalidates native pending presentation callbacks immediately.
        self.set_visible(false);
    }
}

impl Drop for Prototype {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests;
