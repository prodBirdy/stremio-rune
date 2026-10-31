//! Direct2D drawing for the Setup window: a layered, borderless window with a soft shadow,
//! DirectWrite text in Inter (bundled, as the app bundles it), and the app's stroke icons.

use std::collections::HashMap;

use windows::core::{w, Interface, HSTRING};
use windows::Win32::Foundation::{COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_HOLLOW, D2D1_FIGURE_END_OPEN,
    D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1DCRenderTarget, ID2D1Factory, ID2D1SolidColorBrush, ID2D1StrokeStyle,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_ARC_SEGMENT, D2D1_ARC_SIZE_LARGE, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
    D2D1_BITMAP_PROPERTIES, D2D1_CAP_STYLE_ROUND, D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_ELLIPSE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_LINE_JOIN_ROUND, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
    D2D1_SWEEP_DIRECTION_CLOCKWISE, D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFactory5, IDWriteFontCollection, IDWriteTextFormat,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT,
    DWRITE_LINE_SPACING_METHOD_UNIFORM, DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP, DWRITE_WORD_WRAPPING_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, UpdateLayeredWindow, ULW_ALPHA};
use windows_numerics::{Matrix3x2, Vector2};

const SHADOW_ALPHA: f32 = 0.5;
const LOGO_PNG: &[u8] = include_bytes!("../assets/icon.png");
/// The app's own font files (Rune bundles the same three).
const INTER: [&[u8]; 3] = [
    include_bytes!("../assets/fonts/Inter-Regular.ttf"),
    include_bytes!("../assets/fonts/Inter-Medium.ttf"),
    include_bytes!("../assets/fonts/Inter-SemiBold.ttf"),
];
/// If Inter can't be loaded (Windows before 10 1703).
const FALLBACK_FAMILY: &str = "Segoe UI Variable Text";

/// The app's icons: stroked polylines on a 24-unit grid with round caps (main.rs `icons`), plus
/// circles as (cx, cy, r). A one-point line is a dot.
pub struct Icon {
    pub lines: &'static [&'static [(f32, f32)]],
    pub circles: &'static [(f32, f32, f32)],
}

pub mod icon {
    use super::Icon;
    const fn lines(lines: &'static [&'static [(f32, f32)]]) -> Icon {
        Icon { lines, circles: &[] }
    }
    pub const CHECK: Icon = lines(&[&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)]]);
    pub const FOLDER: Icon = lines(&[&[(3.0, 6.0), (9.5, 6.0), (11.5, 8.5), (21.0, 8.5), (21.0, 19.0), (3.0, 19.0), (3.0, 6.0)]]);
    pub const DOWNLOAD: Icon = lines(&[&[(12.0, 4.0), (12.0, 15.0)], &[(7.0, 10.5), (12.0, 15.5), (17.0, 10.5)], &[(5.0, 20.0), (19.0, 20.0)]]);
    pub const OPEN: Icon = lines(&[
        &[(14.0, 4.0), (20.0, 4.0), (20.0, 10.0)],
        &[(20.0, 4.0), (11.0, 13.0)],
        &[(18.0, 14.5), (18.0, 20.0), (4.0, 20.0), (4.0, 6.0), (9.5, 6.0)],
    ]);
    pub const DRIVE: Icon = lines(&[&[(3.0, 8.0), (21.0, 8.0), (21.0, 16.0), (3.0, 16.0), (3.0, 8.0)], &[(17.0, 12.0)]]);
    pub const DOCUMENT: Icon = lines(&[&[(6.0, 3.0), (14.0, 3.0), (19.0, 8.0), (19.0, 21.0), (6.0, 21.0), (6.0, 3.0)], &[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)]]);
    pub const WARNING: Icon = lines(&[&[(12.0, 4.0), (21.0, 19.5), (3.0, 19.5), (12.0, 4.0)], &[(12.0, 10.0), (12.0, 13.5)], &[(12.0, 16.6)]]);
    pub const INFO: Icon = Icon { lines: &[&[(12.0, 11.0), (12.0, 16.5)], &[(12.0, 7.6)]], circles: &[(12.0, 12.0, 9.0)] };
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub const fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect { x, y, w, h }
}

impl Rect {
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn inset(&self, d: f32) -> Rect {
        rect(self.x + d, self.y + d, self.w - 2.0 * d, self.h - 2.0 * d)
    }

    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h }
    }
}

#[derive(Clone, Copy)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Colors are `0xRRGGBB`.
pub fn color(c: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((c >> 16) & 0xff) as f32 / 255.0,
        g: ((c >> 8) & 0xff) as f32 / 255.0,
        b: (c & 0xff) as f32 / 255.0,
        a,
    }
}

fn blur_lines(buf: &mut [f32], len: usize, step: usize, lines: usize, line_step: usize, r: usize) {
    let mut prefix = vec![0f32; len + 1];
    let norm = 1.0 / (2 * r + 1) as f32;
    for l in 0..lines {
        let base = l * line_step;
        for i in 0..len {
            prefix[i + 1] = prefix[i] + buf[base + i * step];
        }
        for i in 0..len {
            let lo = i.saturating_sub(r);
            let hi = (i + r + 1).min(len);
            buf[base + i * step] = (prefix[hi] - prefix[lo]) * norm;
        }
    }
}

fn rounded_inside(px: f32, py: f32, r: Rect, radius: f32) -> bool {
    if !r.contains(px, py) {
        return false;
    }
    let cx = px.clamp(r.x + radius, r.x + r.w - radius);
    let cy = py.clamp(r.y + radius, r.y + r.h - radius);
    (px - cx).powi(2) + (py - cy).powi(2) <= radius * radius
}

fn shadow_alpha(w: usize, h: usize, card: Rect, radius: f32, sigma: f32) -> Vec<u8> {
    let mut buf = vec![0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            if rounded_inside(x as f32 + 0.5, y as f32 + 0.5, card, radius) {
                buf[y * w + x] = 1.0;
            }
        }
    }
    let r = sigma.round().max(1.0) as usize;
    for _ in 0..3 {
        blur_lines(&mut buf, w, 1, h, w, r);
        blur_lines(&mut buf, h, w, w, 1, r);
    }
    buf.iter().map(|v| (v * SHADOW_ALPHA * 255.0).round().clamp(0.0, 255.0) as u8).collect()
}

/// The app icon as a premultiplied BGRA square of `px`, box-filtered down from the PNG.
fn logo_bgra(px: u32) -> Option<Vec<u8>> {
    let mut reader = png::Decoder::new(std::io::Cursor::new(LOGO_PNG)).read_info().ok()?;
    let mut data = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut data).ok()?;
    if info.color_type != png::ColorType::Rgba {
        return None;
    }
    let (sw, sh) = (info.width as usize, info.height as usize);
    let side = sw.max(sh) as f32;
    let k = side / px as f32;
    let (ox, oy) = ((side - sw as f32) / 2.0, (side - sh as f32) / 2.0);
    let mut out = Vec::with_capacity((px * px * 4) as usize);
    for ty in 0..px {
        for tx in 0..px {
            let (x0, y0) = (tx as f32 * k - ox, ty as f32 * k - oy);
            // Premultiplied sums, so transparent edge pixels don't darken the average.
            let (mut sum, mut n) = ([0.0f32; 4], 0.0);
            for sy in (y0.floor().max(0.0) as usize)..((y0 + k).ceil().max(0.0) as usize).min(sh) {
                for sx in (x0.floor().max(0.0) as usize)..((x0 + k).ceil().max(0.0) as usize).min(sw) {
                    let p = &data[(sy * sw + sx) * 4..][..4];
                    let a = p[3] as f32 / 255.0;
                    sum[0] += p[2] as f32 * a;
                    sum[1] += p[1] as f32 * a;
                    sum[2] += p[0] as f32 * a;
                    sum[3] += p[3] as f32;
                    n += 1.0;
                }
            }
            out.extend(sum.map(|v| if n > 0.0 { (v / n).round() as u8 } else { 0 }));
        }
    }
    Some(out)
}

/// Inter from the bundled files, as its own font collection.
unsafe fn inter() -> windows::core::Result<IDWriteFontCollection> {
    let f: IDWriteFactory5 = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
    let loader = f.CreateInMemoryFontFileLoader()?;
    f.RegisterFontFileLoader(&loader)?;
    let builder = f.CreateFontSetBuilder()?;
    for data in INTER {
        let file = loader.CreateInMemoryFontFileReference(&f, data.as_ptr().cast(), data.len() as u32, None)?;
        builder.AddFontFile(&file)?;
    }
    f.CreateFontCollectionFromFontSet(&builder.CreateFontSet()?)?.cast()
}

pub struct Gfx {
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    fonts: IDWriteFontCollection,
    family: HSTRING,
    pub rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    round: Option<ID2D1StrokeStyle>,
    formats: HashMap<(u32, u16, bool), IDWriteTextFormat>,
    logo: Option<(u32, ID2D1Bitmap)>,
    shadow: Option<((i32, i32), ID2D1Bitmap)>,
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    size: (i32, i32),
}

impl Drop for Gfx {
    fn drop(&mut self) {
        unsafe {
            if !self.dc.is_invalid() {
                SelectObject(self.dc, self.old);
                let _ = DeleteObject(self.bmp.into());
                let _ = DeleteDC(self.dc);
            }
        }
    }
}

impl Gfx {
    pub unsafe fn new() -> Result<Self, String> {
        let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
            .map_err(|e| format!("D2D1CreateFactory: {e}"))?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = factory.CreateDCRenderTarget(&props).map_err(|e| format!("CreateDCRenderTarget: {e}"))?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let brush = rt.CreateSolidColorBrush(&color(0, 1.0), None).map_err(|e| format!("CreateSolidColorBrush: {e}"))?;
        let dwrite: IDWriteFactory =
            DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).map_err(|e| format!("DWriteCreateFactory: {e}"))?;
        let (fonts, family) = match inter() {
            Ok(c) => (c, HSTRING::from("Inter")),
            Err(_) => {
                let mut fonts: Option<IDWriteFontCollection> = None;
                dwrite.GetSystemFontCollection(&mut fonts, false).map_err(|e| format!("GetSystemFontCollection: {e}"))?;
                (fonts.ok_or("GetSystemFontCollection returned null")?, HSTRING::from(FALLBACK_FAMILY))
            }
        };
        let round = factory
            .CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    ..Default::default()
                },
                None,
            )
            .ok();
        Ok(Gfx {
            factory,
            dwrite,
            fonts,
            family,
            rt,
            brush,
            round,
            formats: HashMap::new(),
            logo: None,
            shadow: None,
            dc: HDC::default(),
            bmp: HBITMAP::default(),
            old: HGDIOBJ::default(),
            size: (0, 0),
        })
    }

    unsafe fn ensure_surface(&mut self, size: (i32, i32)) -> Result<(), String> {
        if self.size == size && !self.dc.is_invalid() {
            return Ok(());
        }
        if !self.dc.is_invalid() {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
            self.dc = HDC::default();
        }
        let dc = CreateCompatibleDC(None);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size.0,
                biHeight: -size.1,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let bmp = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            .map_err(|e| format!("CreateDIBSection: {e}"))?;
        self.old = SelectObject(dc, bmp.into());
        self.dc = dc;
        self.bmp = bmp;
        self.size = size;
        self.shadow = None;
        Ok(())
    }

    /// `weight` as in CSS (400 regular … 700 bold); `paragraph` wraps, else one centred line.
    unsafe fn format(&mut self, px: f32, weight: u16, paragraph: bool) -> Option<IDWriteTextFormat> {
        let key = ((px * 10.0).round() as u32, weight, paragraph);
        if let Some(f) = self.formats.get(&key) {
            return Some(f.clone());
        }
        let f = self
            .dwrite
            .CreateTextFormat(
                &self.family,
                &self.fonts,
                DWRITE_FONT_WEIGHT(weight as i32),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                px,
                w!(""),
            )
            .ok()?;
        if paragraph {
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_WRAP);
            let _ = f.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, px * 1.45, px * 1.12);
        } else {
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        }
        self.formats.insert(key, f.clone());
        Some(f)
    }

    pub unsafe fn measure(&mut self, s: &str, px: f32, weight: u16) -> f32 {
        let Some(f) = self.format(px, weight, false) else {
            return 0.0;
        };
        let text: Vec<u16> = s.encode_utf16().collect();
        let Ok(layout) = self.dwrite.CreateTextLayout(&text, &f, 10_000.0, 100.0) else {
            return 0.0;
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        if layout.GetMetrics(&mut m).is_err() {
            return 0.0;
        }
        m.widthIncludingTrailingWhitespace
    }

    /// One line, vertically centred in `r`, cut with an ellipsis when it does not fit.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn text(&mut self, s: &str, r: Rect, px: f32, weight: u16, c: u32, alpha: f32, align: Align) {
        let fitted = self.fit(s, r.w, px, weight);
        let Some(f) = self.format(px, weight, false) else {
            return;
        };
        let _ = f.SetTextAlignment(match align {
            Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
            Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
        });
        self.brush.SetColor(&color(c, alpha));
        let text: Vec<u16> = fitted.encode_utf16().collect();
        self.rt.DrawText(&text, &f, &r.d2d(), &self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
    }

    unsafe fn fit(&mut self, text: &str, w: f32, px: f32, weight: u16) -> String {
        if self.measure(text, px, weight) <= w {
            return text.to_string();
        }
        let mut chars: Vec<char> = text.chars().collect();
        while !chars.is_empty() {
            chars.pop();
            let cut: String = chars.iter().collect::<String>().trim_end().to_string() + "\u{2026}";
            if self.measure(&cut, px, weight) <= w {
                return cut;
            }
        }
        String::new()
    }

    /// Wrapped text starting at `(x, y)`; returns its height. `draw: false` only measures.
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn paragraph(&mut self, s: &str, x: f32, y: f32, w: f32, px: f32, weight: u16, c: u32, alpha: f32, draw: bool) -> f32 {
        let Some(f) = self.format(px, weight, true) else {
            return 0.0;
        };
        let text: Vec<u16> = s.encode_utf16().collect();
        let Ok(layout) = self.dwrite.CreateTextLayout(&text, &f, w, 100_000.0) else {
            return 0.0;
        };
        let mut m = DWRITE_TEXT_METRICS::default();
        let _ = layout.GetMetrics(&mut m);
        if draw {
            self.brush.SetColor(&color(c, alpha));
            self.rt.DrawTextLayout(Vector2 { X: x, Y: y }, &layout, &self.brush, D2D1_DRAW_TEXT_OPTIONS_CLIP);
        }
        m.height
    }

    /// An icon from `icon` in `r`, stroked like the app's (2.4 units of its 24-unit grid).
    pub unsafe fn icon(&self, icon: &Icon, r: Rect, c: u32, alpha: f32) {
        let k = r.w.min(r.h) / 24.0;
        let at = |(x, y): (f32, f32)| Vector2 { X: r.x + x * k, Y: r.y + y * k };
        let width = 2.4 * k;
        self.brush.SetColor(&color(c, alpha));
        for line in icon.lines {
            if let [p] = line {
                let p = at(*p);
                self.rt.FillEllipse(&D2D1_ELLIPSE { point: p, radiusX: width * 0.6, radiusY: width * 0.6 }, &self.brush);
                continue;
            }
            for pair in line.windows(2) {
                self.rt.DrawLine(at(pair[0]), at(pair[1]), &self.brush, width, self.round.as_ref());
            }
        }
        for &(cx, cy, rad) in icon.circles {
            let e = D2D1_ELLIPSE { point: at((cx, cy)), radiusX: rad * k, radiusY: rad * k };
            self.rt.DrawEllipse(&e, &self.brush, width, None);
        }
    }

    /// A thin line, for the caption's ✕ (drawn 1 px like Windows' own).
    pub unsafe fn line(&self, a: (f32, f32), b: (f32, f32), width: f32, c: u32, alpha: f32) {
        self.brush.SetColor(&color(c, alpha));
        self.rt.DrawLine(Vector2 { X: a.0, Y: a.1 }, Vector2 { X: b.0, Y: b.1 }, &self.brush, width, None);
    }

    pub unsafe fn fill(&self, r: Rect, radius: f32, c: u32, alpha: f32) {
        self.brush.SetColor(&color(c, alpha));
        self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius }, &self.brush);
    }

    pub unsafe fn ring(&self, r: Rect, radius: f32, width: f32, c: u32, alpha: f32) {
        self.brush.SetColor(&color(c, alpha));
        let half = width / 2.0;
        self.rt.DrawRoundedRectangle(
            &D2D1_ROUNDED_RECT { rect: r.inset(half).d2d(), radiusX: (radius - half).max(0.0), radiusY: (radius - half).max(0.0) },
            &self.brush,
            width,
            None,
        );
    }

    pub unsafe fn circle(&self, cx: f32, cy: f32, radius: f32, c: u32, alpha: f32) {
        self.brush.SetColor(&color(c, alpha));
        self.rt.FillEllipse(&D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius }, &self.brush);
    }

    /// A three-quarter ring starting at `turn` (0..1), for the busy spinner.
    pub unsafe fn spinner(&self, cx: f32, cy: f32, radius: f32, turn: f32, c: u32) {
        let Ok(path) = self.factory.CreatePathGeometry() else {
            return;
        };
        let Ok(sink) = path.Open() else {
            return;
        };
        let at = |t: f32| {
            let a = t * std::f32::consts::TAU;
            Vector2 { X: cx + radius * a.cos(), Y: cy + radius * a.sin() }
        };
        sink.BeginFigure(at(turn), D2D1_FIGURE_BEGIN_HOLLOW);
        sink.AddArc(&D2D1_ARC_SEGMENT {
            point: at(turn + 0.75),
            size: D2D_SIZE_F { width: radius, height: radius },
            rotationAngle: 0.0,
            sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
            arcSize: D2D1_ARC_SIZE_LARGE,
        });
        sink.EndFigure(D2D1_FIGURE_END_OPEN);
        let _ = sink.Close();
        self.brush.SetColor(&color(c, 1.0));
        self.rt.DrawGeometry(&path, &self.brush, 2.0, self.round.as_ref());
    }

    pub unsafe fn logo(&mut self, r: Rect, scale: f32) {
        let px = (r.w * scale).round().max(1.0) as u32;
        if !matches!(&self.logo, Some((p, _)) if *p == px) {
            self.logo = logo_bgra(px).and_then(|pixels| self.bitmap(px as i32, px as i32, &pixels).ok()).map(|b| (px, b));
        }
        if let Some((_, bmp)) = &self.logo {
            self.rt.DrawBitmap(bmp, Some(&r.d2d()), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
        }
    }

    unsafe fn bitmap(&self, w: i32, h: i32, bgra: &[u8]) -> Result<ID2D1Bitmap, String> {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        self.rt
            .CreateBitmap(D2D_SIZE_U { width: w as u32, height: h as u32 }, Some(bgra.as_ptr() as *const _), (w * 4) as u32, &props)
            .map_err(|e| format!("CreateBitmap: {e}"))
    }

    /// Soft drop shadow under the card, computed once per surface size.
    pub unsafe fn draw_shadow(&mut self, card: Rect, radius: f32, sigma: f32) {
        let (w, h) = self.size;
        if !matches!(&self.shadow, Some((k, _)) if *k == (w, h)) {
            let alpha = shadow_alpha(w as usize, h as usize, card, radius, sigma);
            let bgra: Vec<u8> = alpha.iter().flat_map(|a| [0, 0, 0, *a]).collect();
            self.shadow = self.bitmap(w, h, &bgra).ok().map(|b| ((w, h), b));
        }
        if let Some((_, bmp)) = &self.shadow {
            self.rt.DrawBitmap(bmp, Some(&rect(0.0, 0.0, w as f32, h as f32).d2d()), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
        }
    }

    pub unsafe fn clip(&self, r: Rect) {
        self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
    }

    pub unsafe fn unclip(&self) {
        self.rt.PopAxisAlignedClip();
    }

    pub unsafe fn begin(&mut self, size: (i32, i32)) -> Result<(), String> {
        self.ensure_surface(size)?;
        let bounds = RECT { left: 0, top: 0, right: size.0, bottom: size.1 };
        self.rt.BindDC(self.dc, &bounds).map_err(|e| format!("BindDC: {e}"))?;
        self.rt.BeginDraw();
        self.rt.SetTransform(&Matrix3x2::identity());
        self.rt.Clear(Some(&color(0, 0.0)));
        Ok(())
    }

    pub unsafe fn transform(&self, scale: f32, dx: f32, dy: f32) {
        self.rt.SetTransform(&Matrix3x2 { M11: scale, M12: 0.0, M21: 0.0, M22: scale, M31: dx, M32: dy });
    }

    pub unsafe fn present(&mut self, hwnd: HWND) -> Result<(), String> {
        self.rt.EndDraw(None, None).map_err(|e| format!("EndDraw: {e}"))?;
        let mut wr = RECT::default();
        let _ = GetWindowRect(hwnd, &mut wr);
        let pos = POINT { x: wr.left, y: wr.top };
        let sz = SIZE { cx: self.size.0, cy: self.size.1 };
        let src = POINT::default();
        let blend = BLENDFUNCTION { BlendOp: 0, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: 1 };
        let screen = GetDC(None);
        let res = UpdateLayeredWindow(hwnd, Some(screen), Some(&pos), Some(&sz), Some(self.dc), Some(&src), COLORREF(0), Some(&blend), ULW_ALPHA);
        ReleaseDC(None, screen);
        res.map_err(|e| format!("UpdateLayeredWindow: {e}"))
    }
}
