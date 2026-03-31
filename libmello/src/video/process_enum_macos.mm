#include "process_enum.hpp"
#include "../util/log.hpp"

#ifdef __APPLE__

#import <AppKit/AppKit.h>
#import <CoreGraphics/CoreGraphics.h>
#include <libproc.h>

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
struct KnownGame {
    const char* name;
    const char* bundle_prefix; // CFBundleIdentifier prefix match
};

static const KnownGame KNOWN_GAMES_MAC[] = {
    {"Minecraft",         "com.mojang.minecraft"},
    {"Steam",             "com.valvesoftware.steam"},
    {"League of Legends", "com.riotgames.LeagueofLegends"},
    {"Roblox",            "com.roblox.RobloxPlayer"},
    {"World of Warcraft", "com.blizzard.worldofwarcraft"},
    {"Diablo IV",         "com.blizzard.DiabloIV"},
};

std::vector<GameProcess> enumerate_game_processes() {
    @autoreleasepool {
        // NSRunningApplication property access (activationPolicy, bundleIdentifier, etc.)
        // requires the main thread — AppKit lazily resolves them via Launch Services.
        __block std::vector<GameProcess> result;

        auto do_enumerate = ^{
            @autoreleasepool {
                NSArray<NSRunningApplication*>* apps = [[NSWorkspace sharedWorkspace] runningApplications];
                if (!apps) return;

                for (NSRunningApplication* app in apps) {
                    if (app.activationPolicy != NSApplicationActivationPolicyRegular) continue;

                    NSString* bundleId = app.bundleIdentifier;
                    if (!bundleId) continue;

                    for (const auto& game : KNOWN_GAMES_MAC) {
                        if ([bundleId hasPrefix:[NSString stringWithUTF8String:game.bundle_prefix]]) {
                            GameProcess gp;
                            gp.pid  = (uint32_t)app.processIdentifier;
                            gp.name = game.name;
                            gp.exe  = app.localizedName ? [app.localizedName UTF8String] : game.name;
                            gp.is_fullscreen = false;
                            result.push_back(std::move(gp));
                            break;
                        }
                    }
                }
            }
        };

        if ([NSThread isMainThread]) {
            do_enumerate();
        } else {
            dispatch_sync(dispatch_get_main_queue(), do_enumerate);
        }

        MELLO_LOG_DEBUG(TAG, "Enumerated %zu game processes", result.size());
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
