#if defined(__linux__)
#include "device_enumerator.hpp"
#include "pipewire_stream.hpp"
#include "../util/log.hpp"
#include <pipewire/extensions/metadata.h>
#include <cstring>
#include <string>
#include <vector>

namespace mello::audio {
namespace {

struct Node {
    std::string name;        // node.name: stable, used as the device id
    std::string description; // node.description: what the user reads
    bool is_source = false;
};

/// One registry pass: the audio nodes and the default sink and source.
/// Runs on a short-lived thread loop; every callback runs on that loop's
/// thread with the loop lock held.
struct RegistryScan {
    pw_thread_loop* loop = nullptr;
    pw_context* context = nullptr;
    pw_core* core = nullptr;
    pw_registry* registry = nullptr;
    pw_metadata* metadata = nullptr;
    spa_hook core_listener{};
    spa_hook registry_listener{};
    spa_hook metadata_listener{};
    pw_core_events core_events{};
    pw_registry_events registry_events{};
    pw_metadata_events metadata_events{};
    int pending_seq = -1;
    bool synced = false;

    std::vector<Node> nodes;
    std::string default_sink;
    std::string default_source;

    static void on_global(void* data, uint32_t id, uint32_t /*permissions*/, const char* type,
                          uint32_t /*version*/, const spa_dict* props) {
        auto* self = static_cast<RegistryScan*>(data);
        if (!props) return;
        if (std::strcmp(type, PW_TYPE_INTERFACE_Node) == 0) {
            const char* cls = spa_dict_lookup(props, PW_KEY_MEDIA_CLASS);
            const char* name = spa_dict_lookup(props, PW_KEY_NODE_NAME);
            if (!cls || !name) return;
            const bool source = std::strcmp(cls, "Audio/Source") == 0;
            const bool sink = std::strcmp(cls, "Audio/Sink") == 0;
            if (!source && !sink) return;
            const char* desc = spa_dict_lookup(props, PW_KEY_NODE_DESCRIPTION);
            if (!desc) desc = spa_dict_lookup(props, PW_KEY_NODE_NICK);
            self->nodes.push_back({name, desc ? desc : name, source});
        } else if (std::strcmp(type, PW_TYPE_INTERFACE_Metadata) == 0 && !self->metadata) {
            const char* mname = spa_dict_lookup(props, PW_KEY_METADATA_NAME);
            if (!mname || std::strcmp(mname, "default") != 0) return;
            self->metadata = static_cast<pw_metadata*>(
                pw_registry_bind(self->registry, id, type, PW_VERSION_METADATA, 0));
            if (self->metadata) {
                self->metadata_events.version = PW_VERSION_METADATA_EVENTS;
                self->metadata_events.property = &RegistryScan::on_property;
                pw_metadata_add_listener(self->metadata, &self->metadata_listener,
                                         &self->metadata_events, self);
            }
        }
    }

    static int on_property(void* data, uint32_t /*subject*/, const char* key,
                           const char* /*type*/, const char* value) {
        auto* self = static_cast<RegistryScan*>(data);
        if (!key) return 0;
        // default.audio.* is the device WirePlumber uses now; the
        // default.configured.* keys are the user's choice, which may be absent.
        if (std::strcmp(key, "default.audio.sink") == 0) {
            self->default_sink = pw::parse_default_node_name(value);
        } else if (std::strcmp(key, "default.audio.source") == 0) {
            self->default_source = pw::parse_default_node_name(value);
        }
        return 0;
    }

    static void on_done(void* data, uint32_t id, int seq) {
        auto* self = static_cast<RegistryScan*>(data);
        if (id == PW_ID_CORE && seq == self->pending_seq) {
            self->synced = true;
            pw_thread_loop_signal(self->loop, false);
        }
    }

    /// Round trip to the daemon: every event sent before it has arrived when
    /// this returns. Loop lock held by the caller.
    bool sync() {
        synced = false;
        pending_seq = pw_core_sync(core, PW_ID_CORE, pending_seq);
        for (int i = 0; i < 3 && !synced; ++i) pw_thread_loop_timed_wait(loop, 1);
        return synced;
    }

    bool run() {
        pw::ensure_init();
        loop = pw_thread_loop_new("m3llo-devices", nullptr);
        if (!loop) return false;
        context = pw_context_new(pw_thread_loop_get_loop(loop), nullptr, 0);
        if (!context || pw_thread_loop_start(loop) < 0) return false;

        pw_thread_loop_lock(loop);
        core = pw_context_connect(context, nullptr, 0);
        if (!core) {
            pw_thread_loop_unlock(loop);
            MELLO_LOG_WARN("devices", "PipeWire: cannot connect to the daemon");
            return false;
        }
        core_events.version = PW_VERSION_CORE_EVENTS;
        core_events.done = &RegistryScan::on_done;
        pw_core_add_listener(core, &core_listener, &core_events, this);

        registry = pw_core_get_registry(core, PW_VERSION_REGISTRY, 0);
        registry_events.version = PW_VERSION_REGISTRY_EVENTS;
        registry_events.global = &RegistryScan::on_global;
        pw_registry_add_listener(registry, &registry_listener, &registry_events, this);

        // First round trip: every global. Second: the properties of the
        // metadata object that the first pass bound.
        const bool ok = sync() && sync();
        pw_thread_loop_unlock(loop);
        return ok;
    }

    ~RegistryScan() {
        if (loop) pw_thread_loop_stop(loop);
        if (metadata) pw_proxy_destroy(reinterpret_cast<pw_proxy*>(metadata));
        if (registry) pw_proxy_destroy(reinterpret_cast<pw_proxy*>(registry));
        if (core) pw_core_disconnect(core);
        if (context) pw_context_destroy(context);
        if (loop) pw_thread_loop_destroy(loop);
    }
};

class PipeWireDeviceEnumerator : public AudioDeviceEnumerator {
public:
    std::vector<AudioDeviceInfo> list_capture_devices() override { return list(true); }
    std::vector<AudioDeviceInfo> list_playback_devices() override { return list(false); }

private:
    static std::vector<AudioDeviceInfo> list(bool sources) {
        RegistryScan scan;
        if (!scan.run()) {
            MELLO_LOG_WARN("devices", "PipeWire: device scan incomplete");
        }
        const std::string& def = sources ? scan.default_source : scan.default_sink;
        std::vector<AudioDeviceInfo> out;
        for (const Node& n : scan.nodes) {
            if (n.is_source != sources) continue;
            out.push_back({n.name, n.description, n.name == def});
        }
        MELLO_LOG_INFO("devices", "PipeWire: %zu %s device(s), default=%s", out.size(),
                       sources ? "capture" : "playback", def.empty() ? "none" : def.c_str());
        return out;
    }
};

} // namespace

std::unique_ptr<AudioDeviceEnumerator> create_device_enumerator() {
    return std::make_unique<PipeWireDeviceEnumerator>();
}

} // namespace mello::audio
#endif
