#include "mello.h"
#import <AVFoundation/AVFoundation.h>

extern "C" {

MelloMicPermission mello_mic_permission_status(void) {
    AVAuthorizationStatus status = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeAudio];
    switch (status) {
        case AVAuthorizationStatusAuthorized:
            return MELLO_MIC_GRANTED;
        case AVAuthorizationStatusDenied:
        case AVAuthorizationStatusRestricted:
            return MELLO_MIC_DENIED;
        case AVAuthorizationStatusNotDetermined:
        default:
            return MELLO_MIC_NOT_DETERMINED;
    }
}

void mello_mic_request_permission(MelloMicPermissionCallback callback, void* user_data) {
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
        if (callback) {
            callback(user_data, granted);
        }
    }];
}

}
