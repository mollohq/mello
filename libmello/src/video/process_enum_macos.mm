#include "process_enum.hpp"
#include "../util/log.hpp"

#ifdef __APPLE__

#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <libproc.h>
#include <unistd.h>

#include <unordered_map>

namespace mello::video {

static constexpr const char* TAG = "video/process";

std::vector<MonitorInfo> enumerate_monitors() {
    @autoreleasepool {
        __block std::vector<MonitorInfo> result;

        auto do_enumerate = ^{
            @autoreleasepool {
                NSArray<NSScreen*>* screens = [NSScreen screens];
                if (!screens) return;

                uint32_t idx = 0;
                for (NSScreen* screen in screens) {
                    NSRect frame = screen.frame;
                    MonitorInfo mi;
                    mi.index   = idx;
                    mi.name    = [[screen localizedName] UTF8String];
                    mi.width   = (uint32_t)frame.size.width;
                    mi.height  = (uint32_t)frame.size.height;
                    mi.primary = (idx == 0);
                    result.push_back(std::move(mi));
                    idx++;
                }
            }
        };

        if ([NSThread isMainThread]) {
            do_enumerate();
        } else {
            dispatch_sync(dispatch_get_main_queue(), do_enumerate);
        }

        MELLO_LOG_DEBUG(TAG, "Enumerated %zu monitors", result.size());
        return result;
    }
}

// Known game list for macOS
namespace {

/// The windows of each process at layer 0 (normal windows), from the window
/// server: on screen or not, and whether one covers its display. Needs no
/// permission: the bounds and the owner are public, the titles are not.
struct ProcessWindows {
    bool any = false;
    bool on_screen = false;
    bool covers_display = false;
    std::string title;
    double largest_area = 0;
};

std::unordered_map<pid_t, ProcessWindows> windows_by_pid() {
    std::unordered_map<pid_t, ProcessWindows> out;
    CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionAll | kCGWindowListExcludeDesktopElements,
                                                 kCGNullWindowID);
    if (!list) return out;
    NSArray* windows = CFBridgingRelease(list);
    NSArray<NSScreen*>* screens = [NSScreen screens];
    const CGFloat main_height = screens.count > 0 ? screens[0].frame.size.height : 0;
    for (NSDictionary* w in windows) {
        if ([w[(id)kCGWindowLayer] intValue] != 0) continue;
        const pid_t pid = [w[(id)kCGWindowOwnerPID] intValue];
        CGRect bounds{};
        if (!CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)w[(id)kCGWindowBounds], &bounds)) continue;
        if (bounds.size.width < 64 || bounds.size.height < 64) continue;
        ProcessWindows& pw = out[pid];
        pw.any = true;
        const bool on = [w[(id)kCGWindowIsOnscreen] boolValue];
        if (!on) continue;
        pw.on_screen = true;
        const double area = bounds.size.width * bounds.size.height;
        if (area > pw.largest_area) {
            pw.largest_area = area;
            NSString* name = w[(id)kCGWindowName];
            pw.title = name ? name.UTF8String : "";
        }
        // The window server's bounds are top-left based; the screens' frames
        // are bottom-left based on the main screen.
        for (NSScreen* screen in screens) {
            const NSRect f = screen.frame;
            const CGRect top_left = CGRectMake(f.origin.x, main_height - f.origin.y - f.size.height, f.size.width,
                                               f.size.height);
            if (CGRectContainsRect(bounds, CGRectInset(top_left, 1, 1))) pw.covers_display = true;
        }
    }
    return out;
}

}  // namespace

// Every regular application: the game catalogue in mello-core decides which
// of them is a game (spec 17 section 2.1), as on Windows. A process with no
// app bundle (a command line tool) has no Dock presence and is not one.
std::vector<GameProcess> enumerate_game_processes() {
    @autoreleasepool {
        __block std::vector<GameProcess> result;

        auto do_enumerate = ^{
            @autoreleasepool {
                NSArray<NSRunningApplication*>* apps = [[NSWorkspace sharedWorkspace] runningApplications];
                if (!apps) return;
                const auto windows = windows_by_pid();
                const pid_t self = getpid();

                for (NSRunningApplication* app in apps) {
                    if (app.activationPolicy != NSApplicationActivationPolicyRegular) continue;
                    if (app.processIdentifier == self || app.terminated) continue;
                    NSURL* exe = app.executableURL;
                    if (!exe) continue;

                    GameProcess gp;
                    gp.pid  = (uint32_t)app.processIdentifier;
                    gp.name = app.localizedName ? [app.localizedName UTF8String] : "";
                    gp.exe  = exe.lastPathComponent ? [exe.lastPathComponent UTF8String] : "";
                    gp.path = exe.path ? [exe.path UTF8String] : "";
                    gp.is_foreground = app.isActive;
                    const auto it = windows.find(app.processIdentifier);
                    if (it != windows.end()) {
                        gp.window_title  = it->second.title;
                        gp.is_fullscreen = it->second.covers_display;
                        // Hidden, or every window is off screen (minimized,
                        // or on another Space).
                        gp.is_minimized = app.hidden || (it->second.any && !it->second.on_screen);
                    } else {
                        gp.is_fullscreen = false;
                        gp.is_minimized  = app.hidden;
                    }
                    if (NSDate* launched = app.launchDate) {
                        gp.started_at_ms = (int64_t)([launched timeIntervalSince1970] * 1000.0);
                    }
                    result.push_back(std::move(gp));
                }
            }
        };

        if ([NSThread isMainThread]) {
            do_enumerate();
        } else {
            dispatch_sync(dispatch_get_main_queue(), do_enumerate);
        }

        MELLO_LOG_DEBUG(TAG, "Enumerated %zu application processes", result.size());
        return result;
    }
}

std::vector<VisibleWindow> enumerate_visible_windows() {
    @autoreleasepool {
        std::vector<VisibleWindow> result;

        // CGWindowListCopyWindowInfo returns all on-screen windows
        CFArrayRef window_list = CGWindowListCopyWindowInfo(
            kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements,
            kCGNullWindowID);

        if (!window_list) return result;

        NSArray* windows = (__bridge_transfer NSArray*)window_list;
        for (NSDictionary* info in windows) {
            // Skip windows with no name or from the WindowServer
            NSString* name  = info[(NSString*)kCGWindowName];
            NSNumber* layer = info[(NSString*)kCGWindowLayer];
            NSNumber* pid   = info[(NSString*)kCGWindowOwnerPID];
            NSString* owner = info[(NSString*)kCGWindowOwnerName];
            NSNumber* wid   = info[(NSString*)kCGWindowNumber];

            if (!name || name.length == 0) continue;
            if (layer && layer.intValue != 0) continue; // Only normal layer windows

            VisibleWindow vw;
            vw.hwnd  = (void*)(uintptr_t)wid.unsignedIntValue; // CGWindowID stored as void*
            vw.title = [name UTF8String];
            vw.exe   = owner ? [owner UTF8String] : "";
            vw.pid   = pid ? pid.unsignedIntValue : 0;
            result.push_back(std::move(vw));
        }

        MELLO_LOG_DEBUG(TAG, "Enumerated %zu visible windows", result.size());
        return result;
    }
}

} // namespace mello::video

#endif
