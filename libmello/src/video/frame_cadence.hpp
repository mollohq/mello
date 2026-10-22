#pragma once
#include <cstdint>

namespace mello::video {

/// The one frame-rate rule. Every capture backend throttle (hook, WGC, DXGI)
/// and the pipeline decimator call it, so a frame that a backend passes is a
/// frame the pipeline also passes.
///
/// It is a credit accumulator. `next_deadline_us` is the time the next frame
/// is due. An accepted frame moves it by exactly one interval, never to the
/// arrival time: a reset to the arrival time quantises the output onto the
/// source grid (a 144 fps source then gives 48 fps for a 60 fps target).
///
/// Rules:
/// - A frame within 1/8 of an interval before the deadline is accepted, so an
///   arrival a little early on a vsync grid is not lost.
/// - At most 3/4 of an interval of lateness is kept as credit. That credit is
///   what lets a second throttle with the same rule pass every frame of the
///   first one, at any phase. A larger stall is dropped, not caught up, so a
///   stall never gives a burst of frames.
/// - `next_deadline_us == 0` starts a fresh cadence. A clock that goes back
///   more than one interval also starts a fresh cadence.
/// - `fps == 0` accepts every frame.
///
/// Pure, so it is testable without a capture device. Not thread safe: the
/// caller owns `next_deadline_us`.
inline bool frame_cadence_accepts(uint64_t timestamp_us, uint64_t& next_deadline_us,
                                  uint32_t fps) {
    if (fps == 0) return true;

    const uint64_t interval_us = 1'000'000ULL / fps;
    const uint64_t tolerance_us = interval_us / 8;
    // An accepted frame leaves the deadline at most one interval plus the
    // tolerance after it. A frame earlier than that is a clock that went back.
    if (next_deadline_us == 0 ||
        timestamp_us + interval_us + tolerance_us < next_deadline_us) {
        next_deadline_us = timestamp_us + interval_us;
        return true;
    }

    if (timestamp_us + tolerance_us < next_deadline_us) return false;

    const uint64_t max_credit_us = interval_us * 3 / 4;
    if (timestamp_us > next_deadline_us + max_credit_us) {
        next_deadline_us = timestamp_us - max_credit_us;
    }
    next_deadline_us += interval_us;
    return true;
}

} // namespace mello::video
