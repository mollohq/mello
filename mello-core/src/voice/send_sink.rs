//! SFU voice send path off the command loop (spec 10 section 4.5).
//!
//! libmello calls [`sfu_packet_sink`] on the audio capture thread right after
//! Opus encode. The sink sends the frame on the SFU peer at once. Before, the
//! frame waited in libmello's queue until the 20 ms voice tick on the command
//! loop took it, so a command that held the loop also held the microphone.
//!
//! Lifetime. [`VoiceManager`](super::VoiceManager) keeps the [`SfuPacketSink`]
//! in an `Arc` while libmello holds a pointer to it. It clears the libmello
//! sink before it drops the `Arc`, and the clear waits for a sink call that
//! runs now. The peer pointer has its own guard in
//! [`AudioSendTarget`](crate::transport::AudioSendTarget), so the sink can
//! outlive the connection without a send to a destroyed peer.
//!
//! Lock order on the capture thread: libmello capture lock, libmello sink
//! lock, the send target's read lock, the peer mutex. No thread takes them in
//! another order: the command loop clears the libmello sink and the send
//! target one at a time, never one inside the other.

use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::Arc;

use crate::transport::AudioSendTarget;

/// Opus frame bound for the mic test copy (libmello `MAX_PACKET_SIZE`).
const MAX_OPUS_FRAME: usize = 4000;
/// Sent frames between two debug lines (one minute of speech).
const SENT_LOG_EVERY: u64 = 3000;
/// Skipped frames between two info lines after the first.
const SKIPPED_LOG_EVERY: u64 = 500;

/// State of one registered SFU voice packet sink. One per SFU voice join.
pub(crate) struct SfuPacketSink {
    target: Arc<AudioSendTarget>,
    /// Context to feed a copy of each frame for the mic test (loopback) in a
    /// call. Null when the mic test is off.
    loopback_ctx: AtomicPtr<mello_sys::MelloContext>,
    sent: AtomicU64,
    skipped: AtomicU64,
    /// Test only: a record of every sink call (see [`trace`]).
    #[cfg(test)]
    trace: std::sync::OnceLock<trace::Calls>,
}

impl SfuPacketSink {
    pub(crate) fn new(target: Arc<AudioSendTarget>) -> Self {
        Self {
            target,
            loopback_ctx: AtomicPtr::new(std::ptr::null_mut()),
            sent: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            #[cfg(test)]
            trace: std::sync::OnceLock::new(),
        }
    }

    /// Test only: record every call of this sink into `calls`.
    #[cfg(test)]
    pub(crate) fn trace_into(&self, calls: trace::Calls) {
        let _ = self.trace.set(calls);
    }

    /// Feed each frame also to `ctx` as the `loopback` peer, or stop with
    /// null. `ctx` must stay valid until the libmello sink is cleared.
    pub(crate) fn set_loopback(&self, ctx: *mut mello_sys::MelloContext) {
        self.loopback_ctx.store(ctx, Ordering::Release);
    }

    /// Frames sent on the peer since this sink was set.
    pub(crate) fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Frames the peer did not take since this sink was set.
    pub(crate) fn skipped(&self) -> u64 {
        self.skipped.load(Ordering::Relaxed)
    }

    fn deliver(&self, payload: &[u8], timestamp: u32, sequence: u32) {
        #[cfg(test)]
        let entered = std::time::Instant::now();
        let result = self.target.send(payload, timestamp);
        #[cfg(test)]
        if let Some(calls) = self.trace.get() {
            if let Ok(mut calls) = calls.lock() {
                calls.push(trace::SinkCall {
                    sequence,
                    entered,
                    returned: std::time::Instant::now(),
                    sent: result.is_ok(),
                });
            }
        }
        match result {
            Ok(()) => {
                let n = self.sent.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 {
                    log::info!(
                        "SFU voice: first frame sent from the capture thread (seq={} ts={})",
                        sequence,
                        timestamp
                    );
                } else if n.is_multiple_of(SENT_LOG_EVERY) {
                    log::debug!("SFU voice: {} frames sent from the capture thread", n);
                }
            }
            Err(skip) => {
                let n = self.skipped.fetch_add(1, Ordering::Relaxed) + 1;
                if n == 1 || n.is_multiple_of(SKIPPED_LOG_EVERY) {
                    log::info!(
                        "SFU voice: frame not sent ({}), seq={} skipped={} sent={}",
                        skip,
                        sequence,
                        n,
                        self.sent.load(Ordering::Relaxed)
                    );
                }
            }
        }

        let loopback = self.loopback_ctx.load(Ordering::Acquire);
        if !loopback.is_null() && payload.len() <= MAX_OPUS_FRAME {
            // The receive path takes the 4-byte sequence header of
            // mello_voice_get_packet.
            let mut pkt = [0u8; 4 + MAX_OPUS_FRAME];
            pkt[..4].copy_from_slice(&sequence.to_le_bytes());
            pkt[4..4 + payload.len()].copy_from_slice(payload);
            // SAFETY: the context outlives the libmello sink (see
            // `set_loopback`); feed_packet copies the bytes and does not take
            // the capture lock that this thread holds.
            unsafe {
                mello_sys::mello_voice_feed_packet(
                    loopback,
                    c"loopback".as_ptr(),
                    pkt.as_ptr(),
                    (4 + payload.len()) as i32,
                );
            }
        }
    }
}

/// The libmello packet sink callback. `user_data` is an `SfuPacketSink`.
///
/// # Safety
/// libmello calls it with the `user_data` given to
/// `mello_voice_set_packet_sink`, which must point to a live `SfuPacketSink`
/// until the sink is cleared, and with `data` valid for `size` bytes.
pub(crate) unsafe extern "C" fn sfu_packet_sink(
    user_data: *mut std::ffi::c_void,
    data: *const u8,
    size: i32,
    timestamp: u32,
    sequence: u32,
) {
    if user_data.is_null() || data.is_null() || size <= 0 {
        return;
    }
    let sink = &*(user_data as *const SfuPacketSink);
    let payload = std::slice::from_raw_parts(data, size as usize);
    sink.deliver(payload, timestamp, sequence);
}

/// Test only: the sink calls of each context, for the send path latency
/// measurement (plans/voice-quality.md stage 2 gate).
#[cfg(test)]
pub(crate) mod trace {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Instant;

    /// One sink call: the libmello sequence, the time libmello called the
    /// sink (right after Opus encode), the time the send returned, and
    /// whether the peer took the frame.
    #[derive(Clone, Copy, Debug)]
    pub(crate) struct SinkCall {
        pub sequence: u32,
        pub entered: Instant,
        pub returned: Instant,
        pub sent: bool,
    }

    pub(crate) type Calls = Arc<Mutex<Vec<SinkCall>>>;

    fn all() -> &'static Mutex<HashMap<usize, Calls>> {
        static ALL: OnceLock<Mutex<HashMap<usize, Calls>>> = OnceLock::new();
        ALL.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// The call record of the context at `ctx`. Created on first use.
    pub(crate) fn for_context(ctx: usize) -> Calls {
        Arc::clone(all().lock().expect("sink traces").entry(ctx).or_default())
    }
}
