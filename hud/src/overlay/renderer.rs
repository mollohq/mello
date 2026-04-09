#![cfg(target_os = "windows")]

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::*;
use windows_numerics::Vector2;

use crate::protocol::HudState;

const PANEL_WIDTH: f32 = 220.0;
const PANEL_PADDING: f32 = 10.0;
const ROW_HEIGHT: f32 = 24.0;
const HEADER_HEIGHT: f32 = 22.0;
const SEPARATOR_HEIGHT: f32 = 1.0;
const GAP: f32 = 4.0;
const AVATAR_SIZE: f32 = 18.0;
const FONT_SIZE_SM: f32 = 11.0;
const FONT_SIZE_XS: f32 = 9.0;
const FONT_SIZE_XXS: f32 = 8.0;
const TOAST_HEIGHT: f32 = 24.0;

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
    dc_rt: ID2D1DCRenderTarget,
    rt: ID2D1RenderTarget,
    tf_label: IDWriteTextFormat,
    tf_name: IDWriteTextFormat,
    tf_mono_xs: IDWriteTextFormat,
    tf_mono_xxs: IDWriteTextFormat,
}

impl D2DRenderer {
    pub fn new() -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;

            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };

            let dc_rt = factory.CreateDCRenderTarget(&props)?;
            let rt: ID2D1RenderTarget = dc_rt.clone().into();
            rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);

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
                FONT_SIZE_SM,
                w!("en-US"),
            )?;

            let tf_mono_xs = dwrite.CreateTextFormat(
                w!("JetBrains Mono"),
                None,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                FONT_SIZE_XS,
                w!("en-US"),
            )?;
            tf_mono_xs.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER)?;
            tf_mono_xs.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;

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

            Ok(Self {
                factory,
                _dwrite: dwrite,
                dc_rt,
                rt,
                tf_label,
                tf_name,
                tf_mono_xs,
                tf_mono_xxs,
            })
        }
    }

    pub fn compute_height(&self, state: &HudState) -> u32 {
        let member_count = state.voice.as_ref().map(|v| v.members.len()).unwrap_or(0) as f32;

        let mut h = PANEL_PADDING * 2.0
            + HEADER_HEIGHT
            + SEPARATOR_HEIGHT
            + GAP
            + (member_count * ROW_HEIGHT)
            + ((member_count - 1.0).max(0.0) * GAP);

        if state.clip_toast.is_some() {
            h += GAP + TOAST_HEIGHT;
        }

        h.ceil() as u32
    }

    /// Bind the DC render target to a memory DC and render the overlay content.
    pub fn render(&self, state: &HudState, hdc: HDC, width: u32, height: u32) -> Result<()> {
        unsafe {
            let rc = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            self.dc_rt.BindDC(hdc, &rc)?;

            self.rt.BeginDraw();

            // Clear to fully transparent
            self.rt.Clear(Some(&color(0.0, 0.0, 0.0, 0.0)));

            // Glass panel background
            let panel_bg = self.brush(color(0.0, 0.0, 0.0, 0.45))?;
            let panel_border = self.brush(color(1.0, 1.0, 1.0, 0.08))?;
            let panel_rr = rounded(0.0, 0.0, width as f32, height as f32, 12.0);
            self.rt.FillRoundedRectangle(&panel_rr, &panel_bg);
            self.rt
                .DrawRoundedRectangle(&panel_rr, &panel_border, 1.0, None);

            let mut y = PANEL_PADDING;

            // Header: crew initials + name + online count
            if let Some(ref crew) = state.crew {
                self.draw_header(crew, &mut y, width as f32)?;
            }

            // Separator
            let sep_brush = self.brush(color(1.0, 1.0, 1.0, 0.10))?;
            self.rt.FillRectangle(
                &rect(PANEL_PADDING, y, width as f32 - PANEL_PADDING * 2.0, 1.0),
                &sep_brush,
            );
            y += SEPARATOR_HEIGHT + GAP;

            // Member rows
            if let Some(ref voice) = state.voice {
                for member in &voice.members {
                    self.draw_member_row(member, &mut y, width as f32)?;
                    y += GAP;
                }
            }

            // Clip toast
            if let Some(ref toast) = state.clip_toast {
                y += GAP;
                self.draw_clip_toast(toast, &mut y, width as f32)?;
            }

            self.rt.EndDraw(None, None)?;
        }
        Ok(())
    }

    unsafe fn draw_header(
        &self,
        crew: &crate::protocol::HudCrew,
        y: &mut f32,
        width: f32,
    ) -> Result<()> {
        let x = PANEL_PADDING;

        // Crew initials (accent colored)
        let accent = self.brush(color(1.0, 0.118, 0.337, 1.0))?; // #FF1E56
        let initials_w: Vec<u16> = crew.initials.encode_utf16().collect();
        self.rt.DrawText(
            &initials_w,
            &self.tf_mono_xs,
            &rect(x, *y, 20.0, HEADER_HEIGHT),
            &accent,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        // Crew name
        let white = self.brush(color(1.0, 1.0, 1.0, 1.0))?;
        let name_w: Vec<u16> = crew.name.encode_utf16().collect();
        self.rt.DrawText(
            &name_w,
            &self.tf_label,
            &rect(x + 24.0, *y, width - x - 24.0 - 50.0, HEADER_HEIGHT),
            &white,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        // Online count pill
        let pill_x = width - PANEL_PADDING - 36.0;
        let pill_bg = self.brush(color(0.0, 0.0, 0.0, 0.4))?;
        let pill_border = self.brush(color(1.0, 1.0, 1.0, 0.05))?;
        let pill_rr = rounded(pill_x, *y + 2.0, 36.0, 16.0, 8.0);
        self.rt.FillRoundedRectangle(&pill_rr, &pill_bg);
        self.rt
            .DrawRoundedRectangle(&pill_rr, &pill_border, 1.0, None);

        // Green dot
        let green = self.brush(color(0.063, 0.725, 0.506, 1.0))?; // #10B981
        let dot = Vector2 {
            X: pill_x + 8.0,
            Y: *y + 10.0,
        };
        let ellipse = D2D1_ELLIPSE {
            point: dot,
            radiusX: 3.0,
            radiusY: 3.0,
        };
        self.rt.FillEllipse(&ellipse, &green);

        // Count text
        let count_text = crew.online_count.to_string();
        let count_w: Vec<u16> = count_text.encode_utf16().collect();
        let muted_color = self.brush(color(0.631, 0.631, 0.667, 1.0))?; // #A1A1AA
        self.rt.DrawText(
            &count_w,
            &self.tf_mono_xs,
            &rect(pill_x + 14.0, *y + 2.0, 20.0, 16.0),
            &muted_color,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        *y += HEADER_HEIGHT + GAP;
        Ok(())
    }

    unsafe fn draw_member_row(
        &self,
        member: &crate::protocol::HudVoiceMember,
        y: &mut f32,
        _width: f32,
    ) -> Result<()> {
        let x = PANEL_PADDING + 4.0;
        let row_y = *y;

        // Avatar square
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

        let av_bg_brush = self.brush(av_bg)?;
        let av_border_brush = self.brush(av_border)?;
        let av_rect = rounded(
            x,
            row_y + (ROW_HEIGHT - AVATAR_SIZE) / 2.0,
            AVATAR_SIZE,
            AVATAR_SIZE,
            4.0,
        );
        self.rt.FillRoundedRectangle(&av_rect, &av_bg_brush);
        self.rt
            .DrawRoundedRectangle(&av_rect, &av_border_brush, 1.0, None);

        // Avatar initials
        let av_text_brush = self.brush(av_text_color)?;
        let initials_w: Vec<u16> = member.initials.encode_utf16().collect();
        self.rt.DrawText(
            &initials_w,
            &self.tf_mono_xxs,
            &rect(
                x,
                row_y + (ROW_HEIGHT - AVATAR_SIZE) / 2.0,
                AVATAR_SIZE,
                AVATAR_SIZE,
            ),
            &av_text_brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        // Name
        let name_x = x + AVATAR_SIZE + 8.0;
        let name_color = if member.speaking {
            color(1.0, 1.0, 1.0, 1.0)
        } else if member.muted {
            color(0.631, 0.631, 0.667, 1.0)
        } else {
            color(0.831, 0.831, 0.847, 1.0) // #D4D4D8
        };
        let name_brush = self.brush(name_color)?;
        let name_tf = if member.speaking {
            &self.tf_label
        } else {
            &self.tf_name
        };
        let name_w: Vec<u16> = member.display_name.encode_utf16().collect();
        self.rt.DrawText(
            &name_w,
            name_tf,
            &rect(name_x, row_y, 120.0, ROW_HEIGHT),
            &name_brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        // Speaking bars or muted icon
        let indicator_x = PANEL_WIDTH - PANEL_PADDING - 16.0;
        if member.speaking {
            self.draw_speaking_bars(indicator_x, row_y, ROW_HEIGHT)?;
        } else if member.muted {
            self.draw_mute_indicator(indicator_x, row_y, ROW_HEIGHT)?;
        }

        *y += ROW_HEIGHT;
        Ok(())
    }

    unsafe fn draw_speaking_bars(&self, x: f32, y: f32, row_h: f32) -> Result<()> {
        let green = self.brush(color(0.063, 0.725, 0.506, 1.0))?;
        let bar_w = 2.0;
        let gap = 2.0;
        let center_y = y + row_h / 2.0;

        // Static representation — animation is driven by re-renders with varying heights
        // For now, use fixed heights that look like mid-animation
        let heights = [10.0, 7.0, 10.0];

        for (i, &h) in heights.iter().enumerate() {
            let bx = x + (i as f32) * (bar_w + gap);
            let by = center_y - h / 2.0;
            let bar_rr = rounded(bx, by, bar_w, h, 1.0);
            self.rt.FillRoundedRectangle(&bar_rr, &green);
        }

        Ok(())
    }

    unsafe fn draw_mute_indicator(&self, x: f32, y: f32, row_h: f32) -> Result<()> {
        let red = self.brush(color(1.0, 0.118, 0.337, 1.0))?; // #FF1E56
        let center_y = y + row_h / 2.0;
        let size = 10.0;

        // Draw a simple "X" as a mute indicator (the SVG icon can't be easily
        // rendered in D2D without a separate asset pipeline)
        let x1 = x + 1.0;
        let y1 = center_y - size / 2.0;

        let factory = &self.factory;
        let geom = factory.CreatePathGeometry()?;
        let sink = geom.Open()?;

        // Slash through mic representation
        sink.BeginFigure(Vector2 { X: x1, Y: y1 }, D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddLine(Vector2 {
            X: x1 + size,
            Y: y1 + size,
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        sink.Close()?;

        self.rt.DrawGeometry(&geom, &red, 1.5, None);

        // Small circle for mic body
        let mic_ellipse = D2D1_ELLIPSE {
            point: Vector2 {
                X: x1 + size / 2.0,
                Y: center_y - 1.0,
            },
            radiusX: 3.0,
            radiusY: 4.0,
        };
        self.rt.DrawEllipse(&mic_ellipse, &red, 1.5, None);

        Ok(())
    }

    unsafe fn draw_clip_toast(
        &self,
        toast: &crate::protocol::HudClipToast,
        y: &mut f32,
        width: f32,
    ) -> Result<()> {
        let accent = self.brush(color(1.0, 0.118, 0.337, 0.3))?;
        let accent_border = self.brush(color(1.0, 0.118, 0.337, 0.5))?;
        let toast_rr = rounded(
            PANEL_PADDING,
            *y,
            width - PANEL_PADDING * 2.0,
            TOAST_HEIGHT,
            6.0,
        );
        self.rt.FillRoundedRectangle(&toast_rr, &accent);
        self.rt
            .DrawRoundedRectangle(&toast_rr, &accent_border, 1.0, None);

        let white = self.brush(color(1.0, 1.0, 1.0, 0.9))?;
        let text_w: Vec<u16> = toast.label.encode_utf16().collect();
        self.rt.DrawText(
            &text_w,
            &self.tf_name,
            &rect(
                PANEL_PADDING + 8.0,
                *y,
                width - PANEL_PADDING * 2.0 - 16.0,
                TOAST_HEIGHT,
            ),
            &white,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );

        *y += TOAST_HEIGHT;
        Ok(())
    }

    unsafe fn brush(&self, c: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
        self.rt.CreateSolidColorBrush(&c, None)
    }
}
