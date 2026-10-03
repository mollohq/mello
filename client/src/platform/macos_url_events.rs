//! `mello://` links that macOS sends to the app (CREW-INVITES §6).
//!
//! macOS does not put the URL in argv, and it does not start a second
//! instance for an app that runs. LaunchServices sends a `kAEGetURL` Apple
//! Event to the app instead: at a cold start, and while the app runs. This
//! module receives the event and queues the URL. The poll loop takes the
//! queue on each tick and dispatches the URL as a link that a second
//! instance relays on Windows.

use std::ffi::{c_char, CStr};
use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{class, define_class, msg_send, sel, AllocAnyThread};

const fn four_char_code(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// `kInternetEventClass` and `kAEGetURL`.
const GET_URL: u32 = four_char_code(b"GURL");
/// `keyDirectObject`: the URL in a `kAEGetURL` event.
const DIRECT_OBJECT: u32 = four_char_code(b"----");

/// URLs that macOS sent and the poll loop has not taken yet.
static RECEIVED: Mutex<Vec<String>> = Mutex::new(Vec::new());

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and this class does
    // not implement Drop.
    #[unsafe(super(NSObject))]
    #[name = "MelloUrlEventHandler"]
    struct UrlEventHandler;

    impl UrlEventHandler {
        #[unsafe(method(handleGetURLEvent:withReplyEvent:))]
        fn handle_get_url(&self, event: &AnyObject, _reply: &AnyObject) {
            match url_in(event) {
                Some(url) => {
                    log::info!("[deep-link] macOS sent {url}");
                    RECEIVED
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(url);
                }
                None => log::warn!("[deep-link] macOS sent a URL event with no URL"),
            }
        }

        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _notification: &AnyObject) {
            register(self);
        }
    }
);

/// Receive `kAEGetURL` events from now on. Call once, on the main thread,
/// before the Slint event loop starts.
///
/// AppKit can install its own `kAEGetURL` handler while the app finishes
/// launching. A handler registered before that is replaced, and the URL of a
/// cold start is lost. Apple's advice is to register in
/// `applicationWillFinishLaunching`. winit owns the app delegate, so this
/// registers when that notification arrives, and also now.
pub fn install() {
    let handler: Retained<UrlEventHandler> = unsafe { msg_send![UrlEventHandler::alloc(), init] };
    register(&handler);

    let name = ns_string(c"NSApplicationWillFinishLaunchingNotification");
    unsafe {
        let center: Retained<AnyObject> = msg_send![class!(NSNotificationCenter), defaultCenter];
        let _: () = msg_send![
            &*center,
            addObserver: &*handler,
            selector: sel!(applicationWillFinishLaunching:),
            name: &*name,
            object: Option::<&AnyObject>::None
        ];
    }
    // Neither the event manager nor the notification center retains the
    // handler. It must live as long as the process.
    std::mem::forget(handler);
    log::info!("[deep-link] listening for mello:// URLs from macOS");
}

/// Take the URLs that arrived since the last call.
pub fn take() -> Vec<String> {
    std::mem::take(&mut *RECEIVED.lock().unwrap_or_else(|e| e.into_inner()))
}

fn register(handler: &UrlEventHandler) {
    unsafe {
        let manager: Retained<AnyObject> =
            msg_send![class!(NSAppleEventManager), sharedAppleEventManager];
        let _: () = msg_send![
            &*manager,
            setEventHandler: handler,
            andSelector: sel!(handleGetURLEvent:withReplyEvent:),
            forEventClass: GET_URL,
            andEventID: GET_URL
        ];
    }
}

/// The URL string in a `kAEGetURL` event.
fn url_in(event: &AnyObject) -> Option<String> {
    unsafe {
        let param: Option<Retained<AnyObject>> =
            msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT];
        let string: Option<Retained<AnyObject>> = msg_send![&*param?, stringValue];
        let utf8: *const c_char = msg_send![&*string?, UTF8String];
        if utf8.is_null() {
            return None;
        }
        Some(CStr::from_ptr(utf8).to_string_lossy().into_owned())
    }
}

fn ns_string(text: &CStr) -> Retained<AnyObject> {
    unsafe {
        let string: Option<Retained<AnyObject>> =
            msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()];
        string.expect("NSString from a static C string")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_char_codes_match_the_carbon_constants() {
        // kInternetEventClass = kAEGetURL = 'GURL', keyDirectObject = '----'.
        assert_eq!(GET_URL, 0x4755_524C);
        assert_eq!(DIRECT_OBJECT, 0x2D2D_2D2D);
    }

    /// The URL comes out of a real Apple Event, built the way LaunchServices
    /// builds one. Not through the handler: the queue is global, and the
    /// poll loop in a parallel test can take from it.
    #[test]
    fn the_url_comes_out_of_a_get_url_event() {
        let event: Retained<AnyObject> = unsafe {
            let event: Retained<AnyObject> = msg_send![
                class!(NSAppleEventDescriptor),
                appleEventWithEventClass: GET_URL,
                eventID: GET_URL,
                targetDescriptor: Option::<&AnyObject>::None,
                returnID: -1i16,
                transactionID: 0i32
            ];
            let url = ns_string(c"mello://join/ABCD-1234");
            let descriptor: Retained<AnyObject> =
                msg_send![class!(NSAppleEventDescriptor), descriptorWithString: &*url];
            let _: () =
                msg_send![&*event, setParamDescriptor: &*descriptor, forKeyword: DIRECT_OBJECT];
            event
        };
        assert_eq!(url_in(&event).as_deref(), Some("mello://join/ABCD-1234"));
    }
}
