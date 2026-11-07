#include "ui_sound_mixer.hpp"

#include "../util/log.hpp"

#include <algorithm>
#include <cmath>

namespace mello::audio {

namespace {

int16_t saturate(float v) {
    return static_cast<int16_t>(std::clamp(std::lround(v), -32768L, 32767L));
}

}  // namespace

UiSoundMixer::UiSoundMixer() {
    for (auto& s : sounds_) s.store(nullptr, std::memory_order_relaxed);
}

bool UiSoundMixer::load(uint32_t id, const int16_t* stereo, size_t frames) {
    if (id >= kMaxSounds || !stereo || frames == 0 || frames > kMaxFrames) {
        MELLO_LOG_WARN("ui_sound", "load rejected: id=%u frames=%zu", id, frames);
        return false;
    }
    auto sound = std::make_unique<Sound>();
    sound->pcm.assign(stereo, stereo + frames * kChannels);
    sound->frames = frames;

    std::lock_guard<std::mutex> lock(control_mutex_);
    sounds_[id].store(sound.get(), std::memory_order_release);
    owned_.push_back(std::move(sound));
    MELLO_LOG_INFO("ui_sound", "loaded sound %u: %zu frames (%.2f s)", id, frames,
                   static_cast<double>(frames) / kSampleRate);
    return true;
}

bool UiSoundMixer::loaded(uint32_t id) const {
    return id < kMaxSounds && sounds_[id].load(std::memory_order_acquire) != nullptr;
}

bool UiSoundMixer::play(uint32_t id, float gain) {
    if (!loaded(id)) {
        MELLO_LOG_WARN("ui_sound", "play: sound %u is not loaded", id);
        return false;
    }
    if (!std::isfinite(gain)) gain = 0.0f;
    gain = std::clamp(gain, 0.0f, 1.0f);

    std::lock_guard<std::mutex> lock(control_mutex_);
    const uint64_t w = queue_write_.load(std::memory_order_relaxed);
    const uint64_t r = queue_read_.load(std::memory_order_acquire);
    if (w - r >= kQueueSize) {
        MELLO_LOG_WARN("ui_sound", "play: queue full, sound %u dropped", id);
        return false;
    }
    queue_[w % kQueueSize] = Request{id, gain};
    queue_write_.store(w + 1, std::memory_order_release);
    return true;
}

void UiSoundMixer::take_requests() {
    uint64_t r = queue_read_.load(std::memory_order_relaxed);
    const uint64_t w = queue_write_.load(std::memory_order_acquire);
    for (; r < w; ++r) {
        const Request req = queue_[r % kQueueSize];
        const Sound* sound = sounds_[req.id].load(std::memory_order_acquire);
        if (!sound) continue;

        // A free voice, else the voice that started first.
        Voice* slot = &voices_[0];
        for (auto& v : voices_) {
            if (!v.sound) {
                slot = &v;
                break;
            }
            if (v.order < slot->order) slot = &v;
        }
        *slot = Voice{sound, 0, req.gain, next_order_++};
    }
    queue_read_.store(r, std::memory_order_release);
}

bool UiSoundMixer::mix_into(int16_t* stereo, int16_t* mono, size_t frames) {
    take_requests();

    bool played = false;
    size_t active = 0;
    for (auto& v : voices_) {
        if (!v.sound) continue;
        const size_t n = (std::min)(frames, v.sound->frames - v.pos);
        const int16_t* src = v.sound->pcm.data() + v.pos * kChannels;
        for (size_t i = 0; i < n; ++i) {
            const float l = static_cast<float>(src[2 * i]) * v.gain;
            const float r = static_cast<float>(src[2 * i + 1]) * v.gain;
            stereo[2 * i] = saturate(static_cast<float>(stereo[2 * i]) + l);
            stereo[2 * i + 1] = saturate(static_cast<float>(stereo[2 * i + 1]) + r);
            mono[i] = saturate(static_cast<float>(mono[i]) + 0.5f * (l + r));
        }
        played = played || n > 0;
        v.pos += n;
        if (v.pos >= v.sound->frames) {
            v.sound = nullptr;
        } else {
            ++active;
        }
    }
    active_.store(active, std::memory_order_relaxed);
    return played;
}

}  // namespace mello::audio
