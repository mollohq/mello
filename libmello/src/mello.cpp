#include "mello.h"
#include "context.hpp"
#include <cstring>

extern "C" {

MelloContext* mello_init(void) {
    try {
        auto* ctx = new mello::Context();
        if (!ctx->initialize()) {
            delete ctx;
            return nullptr;
        }
        return reinterpret_cast<MelloContext*>(ctx);
    } catch (...) {
        return nullptr;
    }
}

void mello_destroy(MelloContext* ctx) {
    if (ctx) {
        auto* context = reinterpret_cast<mello::Context*>(ctx);
        context->shutdown();
        delete context;
    }
}

const char* mello_get_error(MelloContext* ctx) {
    if (!ctx) return "Context is null";
    auto* context = reinterpret_cast<mello::Context*>(ctx);
    return context->get_error();
}

MelloResult mello_voice_start_capture(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    // TODO: Implement
    return MELLO_OK;
}

MelloResult mello_voice_stop_capture(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    // TODO: Implement
    return MELLO_OK;
}

void mello_voice_set_mute(MelloContext* ctx, bool muted) {
    if (!ctx) return;
    // TODO: Implement
    (void)muted;
}

bool mello_voice_is_speaking(MelloContext* ctx) {
    if (!ctx) return false;
    // TODO: Implement
    return false;
}

MelloResult mello_stream_start_host(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    // TODO: Implement
    return MELLO_OK;
}

MelloResult mello_stream_stop_host(MelloContext* ctx) {
    if (!ctx) return MELLO_ERROR_INVALID_PARAM;
    // TODO: Implement
    return MELLO_OK;
}

} // extern "C"
