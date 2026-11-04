// The game clip poster JPEG on Apple platforms (CLIPS.md 15.5): ImageIO.
#include "replay_mux.hpp"
#include "../util/log.hpp"

#import <CoreFoundation/CoreFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ImageIO/ImageIO.h>

#include <string>

namespace mello::replay {

namespace {

constexpr const char* TAG = "replay/poster";

}  // namespace

bool write_poster_jpeg(const uint8_t* bgra, uint32_t width, uint32_t height, uint32_t stride,
                       const std::filesystem::path& path) {
    if (!bgra || width == 0 || height == 0 || stride < width * 4) return false;
    @autoreleasepool {
        CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
        // BGRA in memory is little-endian 32-bit ARGB with the alpha ignored.
        CGContextRef context =
            CGBitmapContextCreate(const_cast<uint8_t*>(bgra), width, height, 8, stride, space,
                                  kCGImageAlphaNoneSkipFirst | kCGBitmapByteOrder32Little);
        CGColorSpaceRelease(space);
        if (!context) {
            MELLO_LOG_ERROR(TAG, "poster: no bitmap context for %ux%u", width, height);
            return false;
        }
        CGImageRef image = CGBitmapContextCreateImage(context);
        CGContextRelease(context);
        if (!image) return false;

        const std::string file = path.string();
        CFURLRef url = CFURLCreateFromFileSystemRepresentation(
            kCFAllocatorDefault, reinterpret_cast<const UInt8*>(file.c_str()), static_cast<CFIndex>(file.size()), false);
        CGImageDestinationRef dest = url ? CGImageDestinationCreateWithURL(url, CFSTR("public.jpeg"), 1, nullptr) : nullptr;
        if (url) CFRelease(url);
        bool ok = false;
        if (dest) {
            const float quality = 0.85f;
            CFNumberRef q = CFNumberCreate(kCFAllocatorDefault, kCFNumberFloatType, &quality);
            const void* keys[] = {kCGImageDestinationLossyCompressionQuality};
            const void* values[] = {q};
            CFDictionaryRef props = CFDictionaryCreate(kCFAllocatorDefault, keys, values, 1,
                                                       &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
            CGImageDestinationAddImage(dest, image, props);
            ok = CGImageDestinationFinalize(dest);
            CFRelease(props);
            CFRelease(q);
            CFRelease(dest);
        }
        CGImageRelease(image);
        if (!ok) MELLO_LOG_ERROR(TAG, "poster: JPEG %ux%u to %s failed", width, height, file.c_str());
        return ok;
    }
}

}  // namespace mello::replay
