#include "mello.h"

extern "C" {

MelloMicPermission mello_mic_permission_status(void) {
    return MELLO_MIC_GRANTED;
}

void mello_mic_request_permission(MelloMicPermissionCallback callback, void* user_data) {
    if (callback) {
        callback(user_data, true);
    }
}

}
