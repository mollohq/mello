// Mello compat stubs for the RTC event log events that goog_cc logs.
//
// goog_cc (WebRTC M131) creates these events and gives them to
// Environment::event_log(). Mello does not vendor the RTC event log encoder
// (logging/rtc_event_log), and the default Environment has RtcEventLogNull,
// which discards each event. These stubs keep the upstream constructor
// signatures so that the upstream goog_cc files compile without an edit.
// The upstream headers at the same paths include the event-log encoder;
// the one-line headers under compat/logging/... include this file instead.
//
// If a WebRTC update changes a constructor, the build fails here. Copy the
// new signature from the upstream header.

#ifndef MELLO_COMPAT_RTC_EVENT_STUBS_H_
#define MELLO_COMPAT_RTC_EVENT_STUBS_H_

#include <cstdint>

#include "api/rtc_event_log/rtc_event.h"
#include "api/transport/bandwidth_usage.h"
#include "api/units/data_rate.h"

namespace webrtc {

enum class ProbeFailureReason {
  kInvalidSendReceiveInterval = 0,
  kInvalidSendReceiveRatio,
  kTimeout,
  kLast
};

// One stub per event type. Each keeps its type tag and nothing else.
template <RtcEvent::Type kTypeTag>
class MelloStubRtcEvent : public RtcEvent {
 public:
  static constexpr Type kType = kTypeTag;
  Type GetType() const override { return kType; }
  bool IsConfigEvent() const override { return false; }
};

class RtcEventAlrState final
    : public MelloStubRtcEvent<RtcEvent::Type::AlrStateEvent> {
 public:
  explicit RtcEventAlrState(bool /*in_alr*/) {}
};

class RtcEventBweUpdateDelayBased final
    : public MelloStubRtcEvent<RtcEvent::Type::BweUpdateDelayBased> {
 public:
  RtcEventBweUpdateDelayBased(int32_t /*bitrate_bps*/,
                              BandwidthUsage /*detector_state*/) {}
};

class RtcEventBweUpdateLossBased final
    : public MelloStubRtcEvent<RtcEvent::Type::BweUpdateLossBased> {
 public:
  RtcEventBweUpdateLossBased(int32_t /*bitrate_bps*/,
                             uint8_t /*fraction_loss*/,
                             int32_t /*total_packets*/) {}
};

class RtcEventProbeClusterCreated final
    : public MelloStubRtcEvent<RtcEvent::Type::ProbeClusterCreated> {
 public:
  RtcEventProbeClusterCreated(int32_t /*id*/,
                              int32_t /*bitrate_bps*/,
                              uint32_t /*min_probes*/,
                              uint32_t /*min_bytes*/) {}
};

class RtcEventProbeResultFailure final
    : public MelloStubRtcEvent<RtcEvent::Type::ProbeResultFailure> {
 public:
  RtcEventProbeResultFailure(int32_t /*id*/,
                             ProbeFailureReason /*failure_reason*/) {}
};

class RtcEventProbeResultSuccess final
    : public MelloStubRtcEvent<RtcEvent::Type::ProbeResultSuccess> {
 public:
  RtcEventProbeResultSuccess(int32_t /*id*/, int32_t /*bitrate_bps*/) {}
};

class RtcEventRemoteEstimate final
    : public MelloStubRtcEvent<RtcEvent::Type::RemoteEstimateEvent> {
 public:
  RtcEventRemoteEstimate(DataRate /*link_capacity_lower*/,
                         DataRate /*link_capacity_upper*/) {}
};

}  // namespace webrtc

#endif  // MELLO_COMPAT_RTC_EVENT_STUBS_H_
