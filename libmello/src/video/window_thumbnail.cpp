#include "window_thumbnail.hpp"
#include "../util/log.hpp"

#ifdef _WIN32
#include <Windows.h>
#include <algorithm>
#include <vector>

namespace mello::video {

static constexpr const char* TAG = "video/thumbnail";

int capture_window_thumbnail(
    void* hwnd_raw,
    uint32_t max_width, uint32_t max_height,
    uint8_t* rgba_out, uint32_t* out_width, uint32_t* out_height)
{
    HWND hwnd = static_cast<HWND>(hwnd_raw);
    if (!IsWindow(hwnd)) return -1;

    RECT rc{};
    if (!GetClientRect(hwnd, &rc) || rc.right <= 0 || rc.bottom <= 0) return -1;

    int win_w = rc.right;
    int win_h = rc.bottom;

    // Compute scaled size preserving aspect ratio
    float scale = std::min(
        static_cast<float>(max_width) / win_w,
        static_cast<float>(max_height) / win_h
    );
    scale = std::min(scale, 1.0f);
    int thumb_w = std::max(1, static_cast<int>(win_w * scale));
    int thumb_h = std::max(1, static_cast<int>(win_h * scale));

    HDC win_dc = GetDC(hwnd);
    if (!win_dc) return -1;

    HDC capture_dc = CreateCompatibleDC(win_dc);
    HBITMAP capture_bmp = CreateCompatibleBitmap(win_dc, win_w, win_h);
    HGDIOBJ old_bmp = SelectObject(capture_dc, capture_bmp);

    BOOL ok = PrintWindow(hwnd, capture_dc, PW_RENDERFULLCONTENT);
    if (!ok) {
        // Fallback to BitBlt
        BitBlt(capture_dc, 0, 0, win_w, win_h, win_dc, 0, 0, SRCCOPY);
    }

    // Scale to thumbnail size
    HDC thumb_dc = CreateCompatibleDC(win_dc);
    HBITMAP thumb_bmp = CreateCompatibleBitmap(win_dc, thumb_w, thumb_h);
    HGDIOBJ old_thumb = SelectObject(thumb_dc, thumb_bmp);

    SetStretchBltMode(thumb_dc, HALFTONE);
    SetBrushOrgEx(thumb_dc, 0, 0, nullptr);
    StretchBlt(thumb_dc, 0, 0, thumb_w, thumb_h,
               capture_dc, 0, 0, win_w, win_h, SRCCOPY);

    // Read pixels from the scaled bitmap
    BITMAPINFOHEADER bi{};
    bi.biSize        = sizeof(bi);
    bi.biWidth       = thumb_w;
    bi.biHeight      = -thumb_h; // top-down
    bi.biPlanes      = 1;
    bi.biBitCount    = 32;
    bi.biCompression = BI_RGB;

    int result = -1;
    int row_bytes = thumb_w * 4;
    std::vector<uint8_t> bgra(row_bytes * thumb_h);

    if (GetDIBits(thumb_dc, thumb_bmp, 0, static_cast<UINT>(thumb_h),
                  bgra.data(), reinterpret_cast<BITMAPINFO*>(&bi), DIB_RGB_COLORS))
    {
        // BGRA -> RGBA
        for (int i = 0; i < thumb_w * thumb_h; ++i) {
            rgba_out[i * 4 + 0] = bgra[i * 4 + 2]; // R
            rgba_out[i * 4 + 1] = bgra[i * 4 + 1]; // G
            rgba_out[i * 4 + 2] = bgra[i * 4 + 0]; // B
            rgba_out[i * 4 + 3] = 255;              // A
        }
        *out_width  = static_cast<uint32_t>(thumb_w);
        *out_height = static_cast<uint32_t>(thumb_h);
        result = 0;
    }

    SelectObject(thumb_dc, old_thumb);
    DeleteObject(thumb_bmp);
    DeleteDC(thumb_dc);
    SelectObject(capture_dc, old_bmp);
    DeleteObject(capture_bmp);
    DeleteDC(capture_dc);
    ReleaseDC(hwnd, win_dc);

    return result;
}

} // namespace mello::video

#else

namespace mello::video {

int capture_window_thumbnail(void*, uint32_t, uint32_t, uint8_t*, uint32_t*, uint32_t*) {
    return -1;
}

} // namespace mello::video

#endif
