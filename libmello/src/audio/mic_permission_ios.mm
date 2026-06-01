#include "mello.h"
#import <AVFoundation/AVFoundation.h>

// iOS mic permission (IOS-LIBMELLO-PORT §5). Uses AVAudioApplication (iOS 17+; our
// floor is iOS 18) rather than the macOS AVCaptureDevice path, which is the
// microphone authorization API for AVAudioSession-based capture.
extern "C" {

MelloMicPermission mello_mic_permission_status(void) {
    switch ([AVAudioApplication sharedInstance].recordPermission) {
        case AVAudioApplicationRecordPermissionGranted:
            return MELLO_MIC_GRANTED;
        case AVAudioApplicationRecordPermissionDenied:
            return MELLO_MIC_DENIED;
        case AVAudioApplicationRecordPermissionUndetermined:
        default:
            return MELLO_MIC_NOT_DETERMINED;
    }
}

void mello_mic_request_permission(MelloMicPermissionCallback callback, void* user_data) {
    [AVAudioApplication requestRecordPermissionWithCompletionHandler:^(BOOL granted) {
        if (callback) {
            callback(user_data, granted);
        }
    }];
}

}
