#![cfg(target_os = "windows")]

use windows::core::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows_numerics::Vector2;

use base64::Engine;

use crate::hud_manager::HudState;

const PANEL_WIDTH: f32 = 230.0;
const PANEL_PADDING: f32 = 10.0;
const ROW_HEIGHT: f32 = 26.0;
const HEADER_HEIGHT: f32 = 24.0;
const CHANNEL_ROW_HEIGHT: f32 = 16.0;
const SEPARATOR_HEIGHT: f32 = 1.0;
const GAP: f32 = 4.0;
const AVATAR_SIZE: f32 = 18.0;
const CREW_AVATAR_SIZE: f32 = 22.0;
const FONT_SIZE_MD: f32 = 13.0;
const FONT_SIZE_SM: f32 = 12.0;
const FONT_SIZE_XS: f32 = 11.0;
const FONT_SIZE_XXS: f32 = 9.0;
const TOAST_HEIGHT: f32 = 26.0;
const LIVE_BADGE_W: f32 = 38.0;
const LIVE_BADGE_H: f32 = 16.0;

fn color(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: rect(x, y, w, h),
        radiusX: r,
        radiusY: r,
    }
}

pub struct D2DRenderer {
    factory: ID2D1Factory,
    _dwrite: IDWriteFactory,
    tf_label: IDWriteTextFormat,
    tf_name: IDWriteTextFormat,
    tf_name_bold: IDWriteTextFormat,
    tf_channel: IDWriteTextFormat,
    tf_mono_xxs: IDWriteTextFormat,
    tf_badge: IDWriteTextFormat,
}

impl D2DRenderer {
    pub fn new(factory: ID2D1Factory) -> Result<Self> {
        unsafe {
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let tf_label = dwrite.CreateTextFormat(
                w!("Inter"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_SIZE_SM,
                w!("en-US"),
            )?;

            let tf_name = dwrite.CreateTextFormat(
                w!("Inter"),
                None,
                DWRITE_FONT_WEIGHT_MEDIUM,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_SIZE_MD,
                w!("en-US"),
            )?;

            let tf_name_bold = dwrite.CreateTextFormat(
                w!("Inter"),
                None,
                DWRITE_FONT_WEIGHT_SEMI_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_SIZE_MD,
                w!("en-US"),
            )?;

            let tf_channel = dwrite.CreateTextFormat(
                w!("Inter"),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_SIZE_XS,
                w!("en-US"),
            )?;

            let tf_mono_xxs = dwrite.CreateTextFormat(
                w!("JetBrains Mono"),
                None,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_SIZE_XXS,
                w!("en-US"),
            )?;
            tf_mono_xxs.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            tf_mono_xxs.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            let tf_badge = dwrite.CreateTextFormat(
                w!("Inter"),
                None,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                9.0,
                w!("en-US"),
            )?;
            tf_badge.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            tf_badge.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

            Ok(Self {
                factory,
                _dwrite: dwrite,
                tf_label,
                tf_name,
                tf_name_bold,
                tf_channel,
                tf_mono_xxs,
                tf_badge,
            })
        }
    }

    pub fn compute_height(&self, state: &HudState) -> u32 {
        let member_count = state.voice.as_ref().map(|v| v.members.len()).unwrap_or(0) as f32;

        let mut h = PANEL_PADDING * 2.0
            + HEADER_HEIGHT
            + GAP
            + CHANNEL_ROW_HEIGHT
            + GAP
            + SEPARATOR_HEIGHT
            + GAP
            + (member_count * ROW_HEIGHT)
            + ((member_count - 1.0).max(0.0) * GAP);

        if state.clip_toast.is_some() {
            h += GAP + TOAST_HEIGHT;
        }

        h.ceil() as u32
    }

    pub fn render(
        &self,
        rt: &ID2D1RenderTarget,
        state: &HudState,
        width: u32,
        height: u32,
        opacity: f32,
    ) -> Result<()> {
        unsafe {
            rt.BeginDraw();

            rt.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

            let panel_bg = self.brush(rt, color(0.0, 0.0, 0.0, opacity.clamp(0.1, 1.0)))?;
            let panel_border = self.brush(rt, color(1.0, 1.0, 1.0, 0.08))?;
            let panel_rr = rounded(0.0, 0.0, width as f32, height as f32, 10.0);
            rt.FillRoundedRectangle(&panel_rr, &panel_bg);
            rt.DrawRoundedRectangle(&panel_rr, &panel_border, 1.0, None);

            let mut y = PANEL_PADDING;

            if let Some(ref crew) = state.crew {
                self.draw_header(rt, crew, &mut y, width as f32)?;
            }

            if let Some(ref voice) = state.voice {
                self.draw_channel_name(rt, &voice.channel_name, &mut y, width as f32)?;
            }

            y += GAP;
            let sep_brush = self.brush(rt, color(1.0, 1.0, 1.0, 0.10))?;
            rt.FillRectangle(
                &rect(PANEL_PADDING, y, width as f32 - PANEL_PADDING * 2.0, 1.0),
                &sep_brush,
            );
            y += SEPARATOR_HEIGHT + GAP;

            if let Some(ref voice) = state.voice {
                let streamer_name = state.stream_card.as_ref().map(|s| s.streamer.as_str());
                for member in &voice.members {
                    let is_streaming = streamer_name.is_some_and(|s| s == member.display_name);
                    self.draw_member_row(rt, member, is_streaming, &mut y, width as f32)?;
                    y += GAP;
                }
            }

            if let Some(ref toast) = state.clip_toast {
                y += GAP;
                self.draw_clip_toast(rt, toast, &mut y, width as f32)?;
            }

            rt.EndDraw(None, None)?;
        }
        Ok(())
    }

    unsafe fn draw_header(
        &self,
        rt: &ID2D1RenderTarget,
        crew: &crate::hud_manager::HudCrew,
        y: &mut f32,
        width: f32,
    ) -> Result<()> {
        let x = PANEL_PADDING;

        let av_y = *y + (HEADER_HEIGHT - CREW_AVATAR_SIZE) / 2.0;
        let av_radius = 5.0;
        let drew_bitmap = if let Some(ref rgba) = crew.avatar_rgba {
            self.draw_avatar_bitmap(rt, rgba, x, av_y, CREW_AVATAR_SIZE, av_radius)?
        } else {
            false
        };
        if !drew_bitmap {
            let accent = self.brush(rt, color(0.922, 0.302, 0.373, 1.0))?;
            let accent_bg = self.brush(rt, color(0.922, 0.302, 0.373, 0.2))?;
            let av_rr = rounded(x, av_y, CREW_AVATAR_SIZE, CREW_AVATAR_SIZE, av_radius);
            rt.FillRoundedRectangle(&av_rr, &accent_bg);
            rt.DrawRoundedRectangle(&av_rr, &accent, 1.0, None);
            let initials_w: Vec<u16> = crew.initials.encode_utf16().collect();
            rt.DrawText(
                &initials_w,
                &self.tf_mono_xxs,
                &rect(x, av_y, CREW_AVATAR_SIZE, CREW_AVATAR_SIZE),
                &accent,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }

        let name_x = x + CREW_AVATAR_SIZE + 6.0;
        let white = self.brush(rt, color(1.0, 1.0, 1.0, 1.0))?;
        let name_w: Vec<u16> = crew.name.encode_utf16().collect();
        rt.DrawText(
            &name_w,
            &self.tf_label,
            &rect(
                name_x,
                *y,
                (width - PANEL_PADDING - name_x).max(0.0),
                HEADER_HEIGHT,
            ),
            &white,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        *y += HEADER_HEIGHT + GAP;
        Ok(())
    }

    unsafe fn draw_channel_name(
        &self,
        rt: &ID2D1RenderTarget,
        channel_name: &str,
        y: &mut f32,
        width: f32,
    ) -> Result<()> {
        let x = PANEL_PADDING;
        let muted = self.brush(rt, color(0.631, 0.631, 0.667, 0.8))?;

        let label = format!("·:: {} ::·", channel_name);
        let label_w: Vec<u16> = label.encode_utf16().collect();
        rt.DrawText(
            &label_w,
            &self.tf_channel,
            &rect(x, *y, width - x * 2.0, CHANNEL_ROW_HEIGHT),
            &muted,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        *y += CHANNEL_ROW_HEIGHT;
        Ok(())
    }

    unsafe fn draw_member_row(
        &self,
        rt: &ID2D1RenderTarget,
        member: &crate::hud_manager::HudVoiceMember,
        is_streaming: bool,
        y: &mut f32,
        _width: f32,
    ) -> Result<()> {
        let x = PANEL_PADDING + 2.0;
        let row_y = *y;

        let (av_bg, av_border, av_text_color) = if member.speaking {
            (
                color(0.063, 0.725, 0.506, 0.2),
                color(0.063, 0.725, 0.506, 0.5),
                color(1.0, 1.0, 1.0, 1.0),
            )
        } else if member.muted {
            (
                color(0.0, 0.0, 0.0, 0.4),
                color(1.0, 1.0, 1.0, 0.05),
                color(0.443, 0.443, 0.478, 1.0),
            )
        } else {
            (
                color(0.0, 0.0, 0.0, 0.5),
                color(1.0, 1.0, 1.0, 0.1),
                color(0.631, 0.631, 0.667, 1.0),
            )
        };

        let av_x = x;
        let av_y = row_y + (ROW_HEIGHT - AVATAR_SIZE) / 2.0;
        let av_r = 5.0;

        let drew_bitmap = if let Some(ref rgba) = member.avatar_rgba {
            self.draw_avatar_bitmap(rt, rgba, av_x, av_y, AVATAR_SIZE, av_r)?
        } else {
            false
        };

        if !drew_bitmap {
            let av_bg_brush = self.brush(rt, av_bg)?;
            let av_border_brush = self.brush(rt, av_border)?;
            let av_rect = rounded(av_x, av_y, AVATAR_SIZE, AVATAR_SIZE, av_r);
            rt.FillRoundedRectangle(&av_rect, &av_bg_brush);
            rt.DrawRoundedRectangle(&av_rect, &av_border_brush, 1.0, None);

            let av_text_brush = self.brush(rt, av_text_color)?;
            let initials_w: Vec<u16> = member.initials.encode_utf16().collect();
            rt.DrawText(
                &initials_w,
                &self.tf_mono_xxs,
                &rect(av_x, av_y, AVATAR_SIZE, AVATAR_SIZE),
                &av_text_brush,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        } else if member.speaking {
            let border_brush = self.brush(rt, av_border)?;
            let av_rect = rounded(av_x, av_y, AVATAR_SIZE, AVATAR_SIZE, av_r);
            rt.DrawRoundedRectangle(&av_rect, &border_brush, 1.5, None);
        }

        let name_x = x + AVATAR_SIZE + 8.0;
        let name_color = if member.speaking {
            color(1.0, 1.0, 1.0, 1.0)
        } else if member.muted {
            color(0.631, 0.631, 0.667, 1.0)
        } else {
            color(0.831, 0.831, 0.847, 1.0)
        };
        let name_brush = self.brush(rt, name_color)?;
        let name_tf = if member.speaking {
            &self.tf_name_bold
        } else {
            &self.tf_name
        };
        let name_w: Vec<u16> = member.display_name.encode_utf16().collect();
        let name_avail = PANEL_WIDTH - name_x - PANEL_PADDING - 24.0;
        let name_y = row_y + (ROW_HEIGHT - FONT_SIZE_MD) / 2.0 - 1.0;
        rt.DrawText(
            &name_w,
            name_tf,
            &rect(name_x, name_y, name_avail.max(40.0), FONT_SIZE_MD + 4.0),
            &name_brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        if is_streaming {
            self.draw_live_badge(rt, row_y, ROW_HEIGHT)?;
        } else if member.speaking {
            let indicator_x = PANEL_WIDTH - PANEL_PADDING - 20.0;
            self.draw_speaking_bars(rt, indicator_x, row_y, ROW_HEIGHT)?;
        } else if member.muted {
            let indicator_x = PANEL_WIDTH - PANEL_PADDING - 20.0;
            self.draw_mute_indicator(rt, indicator_x, row_y, ROW_HEIGHT)?;
        }

        *y += ROW_HEIGHT;
        Ok(())
    }

    unsafe fn draw_speaking_bars(
        &self,
        rt: &ID2D1RenderTarget,
        x: f32,
        y: f32,
        row_h: f32,
    ) -> Result<()> {
        let green = self.brush(rt, color(0.063, 0.725, 0.506, 1.0))?;
        let bar_w = 3.0;
        let gap = 2.0;
        let center_y = y + row_h / 2.0;

        let heights = [12.0, 8.0, 12.0];

        for (i, &h) in heights.iter().enumerate() {
            let bx = x + (i as f32) * (bar_w + gap);
            let by = center_y - h / 2.0;
            let bar_rr = rounded(bx, by, bar_w, h, 1.5);
            rt.FillRoundedRectangle(&bar_rr, &green);
        }

        Ok(())
    }

    unsafe fn draw_mute_indicator(
        &self,
        rt: &ID2D1RenderTarget,
        x: f32,
        y: f32,
        row_h: f32,
    ) -> Result<()> {
        let red = self.brush(rt, color(0.922, 0.302, 0.373, 1.0))?;
        let center_y = y + row_h / 2.0;
        let size = 12.0;

        let x1 = x + 1.0;
        let y1 = center_y - size / 2.0;

        let factory = &self.factory;
        let geom = factory.CreatePathGeometry()?;
        let sink = geom.Open()?;

        sink.BeginFigure(Vector2 { X: x1, Y: y1 }, D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddLine(Vector2 {
            X: x1 + size,
            Y: y1 + size,
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()?;

        rt.DrawGeometry(&geom, &red, 2.0, None);

        let mic_ellipse = D2D1_ELLIPSE {
            point: Vector2 {
                X: x1 + size / 2.0,
                Y: center_y - 1.0,
            },
            radiusX: 4.0,
            radiusY: 5.0,
        };
        rt.DrawEllipse(&mic_ellipse, &red, 2.0, None);

        Ok(())
    }

    unsafe fn draw_live_badge(&self, rt: &ID2D1RenderTarget, row_y: f32, row_h: f32) -> Result<()> {
        let red = color(0.922, 0.302, 0.373, 1.0);
        let red_brush = self.brush(rt, red)?;

        let badge_w = LIVE_BADGE_W;
        let badge_h = LIVE_BADGE_H;
        let badge_x = PANEL_WIDTH - PANEL_PADDING - badge_w;
        let badge_y = row_y + (row_h - badge_h) / 2.0;

        let badge_rr = rounded(badge_x, badge_y, badge_w, badge_h, badge_h / 2.0);
        rt.DrawRoundedRectangle(&badge_rr, &red_brush, 1.0, None);

        let dot_r = 3.0;
        let dot = D2D1_ELLIPSE {
            point: Vector2 {
                X: badge_x + 8.0,
                Y: badge_y + badge_h / 2.0,
            },
            radiusX: dot_r,
            radiusY: dot_r,
        };
        rt.FillEllipse(&dot, &red_brush);

        let live_text: Vec<u16> = "LIVE".encode_utf16().collect();
        rt.DrawText(
            &live_text,
            &self.tf_badge,
            &rect(badge_x + 13.0, badge_y, badge_w - 15.0, badge_h),
            &red_brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        Ok(())
    }

    unsafe fn draw_clip_toast(
        &self,
        rt: &ID2D1RenderTarget,
        toast: &crate::hud_manager::HudClipToast,
        y: &mut f32,
        width: f32,
    ) -> Result<()> {
        let accent = self.brush(rt, color(0.922, 0.302, 0.373, 0.3))?;
        let accent_border = self.brush(rt, color(0.922, 0.302, 0.373, 0.5))?;
        let toast_rr = rounded(
            PANEL_PADDING,
            *y,
            width - PANEL_PADDING * 2.0,
            TOAST_HEIGHT,
            6.0,
        );
        rt.FillRoundedRectangle(&toast_rr, &accent);
        rt.DrawRoundedRectangle(&toast_rr, &accent_border, 1.0, None);

        let white = self.brush(rt, color(1.0, 1.0, 1.0, 0.9))?;
        let text_w: Vec<u16> = toast.label.encode_utf16().collect();
        rt.DrawText(
            &text_w,
            &self.tf_name,
            &rect(
                PANEL_PADDING + 10.0,
                *y,
                width - PANEL_PADDING * 2.0 - 20.0,
                TOAST_HEIGHT,
            ),
            &white,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        *y += TOAST_HEIGHT;
        Ok(())
    }

    unsafe fn draw_avatar_bitmap(
        &self,
        rt: &ID2D1RenderTarget,
        avatar_rgba: &str,
        x: f32,
        y: f32,
        size: f32,
        radius: f32,
    ) -> Result<bool> {
        let parts: Vec<&str> = avatar_rgba.splitn(3, ':').collect();
        if parts.len() != 3 {
            return Ok(false);
        }
        let w: u32 = parts[0].parse().unwrap_or(0);
        let h: u32 = parts[1].parse().unwrap_or(0);
        if w == 0 || h == 0 {
            return Ok(false);
        }
        let rgba = match base64::engine::general_purpose::STANDARD.decode(parts[2]) {
            Ok(d) => d,
            Err(_) => return Ok(false),
        };
        if rgba.len() != (w * h * 4) as usize {
            return Ok(false);
        }

        let mut bgra = rgba;
        for px in bgra.chunks_exact_mut(4) {
            let (r, g, b, a) = (px[0] as u16, px[1] as u16, px[2] as u16, px[3] as u16);
            px[0] = ((b * a + 127) / 255) as u8;
            px[1] = ((g * a + 127) / 255) as u8;
            px[2] = ((r * a + 127) / 255) as u8;
            px[3] = a as u8;
        }

        let bmp_props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        let d2d_size = D2D_SIZE_U {
            width: w,
            height: h,
        };
        let bmp = rt.CreateBitmap(d2d_size, Some(bgra.as_ptr() as _), w * 4, &bmp_props)?;

        let clip_rr = rounded(x, y, size, size, radius);
        let geom = self.factory.CreateRoundedRectangleGeometry(&clip_rr)?;
        let layer_params = D2D1_LAYER_PARAMETERS {
            contentBounds: rect(x, y, size, size),
            geometricMask: unsafe { std::mem::transmute_copy(&geom) },
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: windows_numerics::Matrix3x2::identity(),
            opacity: 1.0,
            opacityBrush: std::mem::ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS_NONE,
        };
        rt.PushLayer(&layer_params, None);

        let dst = rect(x, y, size, size);
        let src = rect(0.0, 0.0, w as f32, h as f32);
        rt.DrawBitmap(
            &bmp,
            Some(&dst),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            Some(&src),
        );

        rt.PopLayer();
        Ok(true)
    }

    unsafe fn brush(
        &self,
        rt: &ID2D1RenderTarget,
        c: D2D1_COLOR_F,
    ) -> Result<ID2D1SolidColorBrush> {
        rt.CreateSolidColorBrush(&c, None)
    }
}
