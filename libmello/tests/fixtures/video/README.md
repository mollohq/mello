# Video fixtures

H.264 Annex B streams for the Media Foundation decoder tests
(`test_windows_decoders.cpp`). Each access unit starts with an AUD. The tests
drop the AUD, as the live stream has none.

| File | Content |
|---|---|
| `bands_640x360_then_320x180.h264` | 10 access units at 640x360, then 10 at 320x180. Each size starts with an IDR and its SPS. Both heights are not multiples of 16, so the coded height is larger (368, 192). |
| `bands_1920x1080.h264` | 10 access units at 1920x1080, coded at 1088 rows. |

## The picture

| Plane | Value |
|---|---|
| Luma, the left half | 60 |
| Luma, the right half | 120 + (frame % 5) x 16: each picture differs from the one before it |
| Chroma, rows 0 to 31 | U 148, V 108 |
| Chroma, row 32 to the middle | U 108, V 148 |
| Chroma, the bottom half | U 148, V 148 |

A decoder that shows an earlier picture again fails the check on the right
half. Every colour is inside the RGB gamut. The encode path converts NV12 to BGRA
and back, and it clamps a colour outside the gamut. The top band shows a
chroma plane that is read from the wrong row.

## How they were made

libmello's own encoder made them: the host pipeline with the reference clip
source and the Cisco OpenH264 encoder (`MELLO_SOFT_ENCODER=1`), 60 fps,
2000 kbps. No GPL tool is used. To make them again, put the OpenH264 DLL in
`libmello/third_party/openh264/`, build the tests, and run:

```bash
mello_tests --gtest_also_run_disabled_tests --gtest_filter=MfFixtures.*
```

The encoder's rate control depends on timing, so the new files are not
byte-identical to these. The tests check the picture, not the bytes.
