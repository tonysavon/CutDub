//! Modular, composable, reorderable, and serializable video filter pipeline for CutDub.
//! Maps directly to FFmpeg `-vf` filtergraphs for export and also provides fast in-memory
//! RGBA frame processing for real-time dual-panel Output preview.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RotationAngle {
    Rotate90Cw,
    Rotate180,
    Rotate270Cw,
    CustomDegrees,
}

impl RotationAngle {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rotate90Cw => "90° Clockwise",
            Self::Rotate180 => "180°",
            Self::Rotate270Cw => "270° Clockwise (90° CCW)",
            Self::CustomDegrees => "Custom Fine Angle (°)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScalingAlgorithm {
    Bicubic,
    Lanczos,
    Bilinear,
    Spline,
    Area,
    Neighbor,
}

impl ScalingAlgorithm {
    pub const ALL: [Self; 6] = [
        Self::Lanczos,
        Self::Bicubic,
        Self::Spline,
        Self::Area,
        Self::Bilinear,
        Self::Neighbor,
    ];

    pub fn ffmpeg_flag(self) -> &'static str {
        match self {
            Self::Bicubic => "bicubic",
            Self::Lanczos => "lanczos",
            Self::Bilinear => "bilinear",
            Self::Spline => "spline",
            Self::Area => "area",
            Self::Neighbor => "neighbor",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Bicubic => "Bicubic",
            Self::Lanczos => "Lanczos (Sharp)",
            Self::Bilinear => "Bilinear (Fast)",
            Self::Spline => "Natural Bicubic Spline",
            Self::Area => "Area Averaging (Downscale)",
            Self::Neighbor => "Nearest Neighbor (Pixel Art)",
        }
    }
}

/// How the Resize filter specifies target dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ResizeSpecMode {
    /// Explicit pixel dimensions (Width x Height).
    #[default]
    Dimensions,
    /// Relative scale percentage of input dimensions (e.g. 50%, 200%).
    Percentage,
}

impl ResizeSpecMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dimensions => "Pixel Dimensions (W × H)",
            Self::Percentage => "Percentage Scale (%)",
        }
    }
}

/// How aspect ratio is handled when resizing to target dimensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AspectRatioMode {
    /// Keep original aspect ratio by adjusting height/width proportionally.
    #[default]
    KeepAspect,
    /// Stretch freely to exact width and height.
    Stretch,
    /// Fit inside target box and pad remaining space with black bars (Letterbox / Pillarbox).
    FitAndPad,
    /// Fill target box completely and center-crop overflow.
    FillAndCrop,
}

impl AspectRatioMode {
    pub const ALL: [Self; 4] = [
        Self::KeepAspect,
        Self::FitAndPad,
        Self::FillAndCrop,
        Self::Stretch,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::KeepAspect => "Keep Aspect Ratio (Proportional)",
            Self::FitAndPad => "Fit & Letterbox / Pillarbox (Pad Bars)",
            Self::FillAndCrop => "Fill & Center-Crop",
            Self::Stretch => "Stretch to Exact W × H (Ignore Aspect)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DeinterlaceEngine {
    #[default]
    Yadif,
    Bwdif,
}

impl DeinterlaceEngine {
    pub fn label(self) -> &'static str {
        match self {
            Self::Yadif => "YADIF (Standard)",
            Self::Bwdif => "BWDIF (Bob Weaver — High Quality)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FieldParity {
    #[default]
    Auto,
    TopFieldFirst,
    BottomFieldFirst,
}

impl FieldParity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto-detect Parity",
            Self::TopFieldFirst => "Top Field First (TFF)",
            Self::BottomFieldFirst => "Bottom Field First (BFF)",
        }
    }

    pub fn ffmpeg_val(self) -> i32 {
        match self {
            Self::Auto => -1,
            Self::TopFieldFirst => 0,
            Self::BottomFieldFirst => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeinterlaceMode {
    SendFrame,
    SendField,
}

impl DeinterlaceMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::SendFrame => "1x Frame Rate (One Frame per Frame)",
            Self::SendField => "2x Field Rate / Bob (One Frame per Field)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SharpenMode {
    #[default]
    UnsharpMask,
    ContrastAdaptive,
}

impl SharpenMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::UnsharpMask => "Unsharp Mask (Luma + Chroma)",
            Self::ContrastAdaptive => "CAS (Contrast Adaptive Sharpening)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DenoiseMode {
    #[default]
    Hqdn3d,
    AtaDenoise,
}

impl DenoiseMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Hqdn3d => "hqdn3d (3D Spatial + Temporal)",
            Self::AtaDenoise => "atadenoise (Adaptive Temporal Averaging)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConvolutionPreset {
    #[default]
    Emboss,
    SobelEdge,
    LaplaceOutline,
}

impl ConvolutionPreset {
    pub const ALL: [Self; 3] = [Self::Emboss, Self::SobelEdge, Self::LaplaceOutline];

    pub fn label(self) -> &'static str {
        match self {
            Self::Emboss => "Emboss (3D Relief)",
            Self::SobelEdge => "Sobel Edge Detect",
            Self::LaplaceOutline => "Laplace Outline (3×3)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OverlayPosition {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    #[default]
    BottomCenter,
    BottomRight,
}

impl OverlayPosition {
    pub const ALL: [Self; 6] = [
        Self::TopLeft,
        Self::TopCenter,
        Self::TopRight,
        Self::BottomLeft,
        Self::BottomCenter,
        Self::BottomRight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::TopLeft => "Top Left",
            Self::TopCenter => "Top Center",
            Self::TopRight => "Top Right",
            Self::BottomLeft => "Bottom Left",
            Self::BottomCenter => "Bottom Center",
            Self::BottomRight => "Bottom Right",
        }
    }
}

fn default_scale_pct() -> f32 {
    100.0
}
fn default_gamma() -> f32 {
    1.0
}
fn default_radius() -> u32 {
    5
}
fn default_temporal_luma() -> f32 {
    6.0
}
fn default_temporal_chroma() -> f32 {
    4.5
}

/// Supported video filter operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FilterKind {
    Crop {
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
    },
    Resize {
        width: u32,
        height: u32,
        algorithm: ScalingAlgorithm,
        #[serde(default)]
        spec_mode: ResizeSpecMode,
        #[serde(default = "default_scale_pct")]
        scale_percent: f32,
        #[serde(default)]
        aspect_mode: AspectRatioMode,
    },
    Rotate {
        angle: RotationAngle,
        #[serde(default)]
        custom_degrees: f32,
        #[serde(default)]
        bilinear: bool,
    },
    Flip {
        horizontal: bool,
        vertical: bool,
    },
    BrightnessContrast {
        /// Range: -1.0 to 1.0 (default 0.0)
        brightness: f32,
        /// Range: 0.0 to 3.0 (default 1.0)
        contrast: f32,
        /// Midtone gamma: 0.2 to 3.0 (default 1.0)
        #[serde(default = "default_gamma")]
        gamma: f32,
    },
    ColorAdjust {
        /// Range: 0.0 to 3.0 (default 1.0)
        saturation: f32,
        /// Range: 0.1 to 5.0 (default 1.0)
        gamma: f32,
        /// Degrees: -180.0 to 180.0 (default 0.0)
        hue_degrees: f32,
        /// Warm / Cool shift: -1.0 (cool blue) to +1.0 (warm amber), default 0.0
        #[serde(default)]
        temperature: f32,
        /// Per-channel RGB gamma/gain offsets (-0.5 to +0.5, default 0.0)
        #[serde(default)]
        r_offset: f32,
        #[serde(default)]
        g_offset: f32,
        #[serde(default)]
        b_offset: f32,
    },
    Deinterlace {
        mode: DeinterlaceMode,
        #[serde(default)]
        engine: DeinterlaceEngine,
        #[serde(default)]
        parity: FieldParity,
    },
    Sharpen {
        /// Range: 0.0 to 3.0 (default 1.0)
        amount: f32,
        #[serde(default)]
        mode: SharpenMode,
        /// Matrix size (3, 5, or 7)
        #[serde(default = "default_radius")]
        radius: u32,
        /// Chroma sharpening (-1.0 to 2.0, default 0.0)
        #[serde(default)]
        chroma_amount: f32,
    },
    Denoise {
        /// Spatial luma strength: 0.0 to 16.0 (default 4.0)
        luma_spatial: f32,
        /// Spatial chroma strength: 0.0 to 16.0 (default 3.0)
        chroma_spatial: f32,
        /// Temporal luma strength: 0.0 to 24.0 (default 6.0)
        #[serde(default = "default_temporal_luma")]
        luma_temporal: f32,
        /// Temporal chroma strength: 0.0 to 24.0 (default 4.5)
        #[serde(default = "default_temporal_chroma")]
        chroma_temporal: f32,
        #[serde(default)]
        mode: DenoiseMode,
    },
    Grayscale,
    Vignette {
        /// Lens vignette angle in radians (0.1 to 1.2, default 0.4)
        angle_rad: f32,
        /// True to brighten corners instead of darkening
        #[serde(default)]
        backward: bool,
    },
    TemporalSpeed {
        /// Playback / export speed factor (`0.25x` .. `4.0x`, default `1.0`)
        speed_factor: f32,
        /// Optional target framerate conversion (e.g. `Some(24.0)`, `Some(30.0)`, `Some(60.0)`)
        #[serde(default)]
        target_fps: Option<f32>,
    },
    /// Classic VirtualDub `levels` filter: input black/white point, gamma, output black/white point.
    Levels {
        input_black: u8,
        input_white: u8,
        gamma: f32,
        output_black: u8,
        output_white: u8,
        #[serde(default)]
        luma_only: bool,
    },
    /// Classic VirtualDub `HSV adjust` filter: Hue (°), Saturation, Value/Lightness.
    HsvAdjust {
        hue_degrees: f32,
        saturation: f32,
        value: f32,
    },
    /// Classic VirtualDub `blur` / `blur more` / `Gaussian blur`.
    Blur {
        sigma: f32,
        steps: u32,
    },
    /// Classic VirtualDub `invert` filter (per-channel or full RGB negation).
    Invert {
        invert_r: bool,
        invert_g: bool,
        invert_b: bool,
    },
    /// Classic VirtualDub `threshold` binary black/white luminance threshold (`0..=255`).
    Threshold {
        level: u8,
    },
    /// Classic VirtualDub `emboss` / `sobel` / `laplace` 3×3 convolution filter.
    EmbossEdge {
        preset: ConvolutionPreset,
        strength: f32,
    },
    /// Classic VirtualDub `2:1 reduction` & `2:1 reduction (high quality)` half-dimension downsample.
    TwoToOneReduction {
        high_quality: bool,
    },
    /// Classic VirtualDub `fill` / `letterbox / border` filter.
    BorderFill {
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
        color_rgb: [u8; 3],
        expand_canvas: bool,
    },
    /// Classic VirtualDub `field swap` (swaps even and odd scanlines).
    FieldSwap,
    /// Timecode, Frame Counter & Custom Text Watermark overlay (`drawtext`).
    TextOverlay {
        text: String,
        show_timecode: bool,
        show_frame_number: bool,
        position: OverlayPosition,
        font_size: u32,
        box_opacity: f32,
    },
    /// Subtitle Burn-In (`subtitles` filter from external `.srt`/`.ass`/`.vtt` file).
    SubtitleBurnIn {
        subtitle_path: String,
        font_size: u32,
    },
}

impl FilterKind {
    /// Helper constructor for `Resize` with default aspect lock and 100% scale.
    pub fn new_resize(width: u32, height: u32, algorithm: ScalingAlgorithm) -> Self {
        Self::Resize {
            width,
            height,
            algorithm,
            spec_mode: ResizeSpecMode::Dimensions,
            scale_percent: 100.0,
            aspect_mode: AspectRatioMode::KeepAspect,
        }
    }

    /// Computes the effective target `(out_w, out_h)` for a `Resize` filter given `(in_w, in_h)`.
    pub fn compute_resize_dimensions(
        in_w: u32,
        in_h: u32,
        width: u32,
        height: u32,
        spec_mode: ResizeSpecMode,
        scale_percent: f32,
        aspect_mode: AspectRatioMode,
    ) -> (u32, u32) {
        let in_w = in_w.max(2);
        let in_h = in_h.max(2);
        match spec_mode {
            ResizeSpecMode::Percentage => {
                let factor = (scale_percent / 100.0).clamp(0.05, 8.0);
                let w = ((in_w as f32 * factor).round() as u32).max(2) & !1;
                let h = ((in_h as f32 * factor).round() as u32).max(2) & !1;
                (w, h)
            }
            ResizeSpecMode::Dimensions => {
                let req_w = width.max(2) & !1;
                let req_h = height.max(2) & !1;
                match aspect_mode {
                    AspectRatioMode::KeepAspect => {
                        // Fit proportionally inside req_w x req_h preserving source aspect ratio
                        let scale_w = req_w as f64 / in_w as f64;
                        let scale_h = req_h as f64 / in_h as f64;
                        let scale = scale_w.min(scale_h);
                        let w = ((in_w as f64 * scale).round() as u32).max(2) & !1;
                        let h = ((in_h as f64 * scale).round() as u32).max(2) & !1;
                        (w, h)
                    }
                    AspectRatioMode::Stretch
                    | AspectRatioMode::FitAndPad
                    | AspectRatioMode::FillAndCrop => (req_w, req_h),
                }
            }
        }
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::Crop { .. } => "Crop",
            Self::Resize { .. } => "Resize / Scale",
            Self::Rotate { .. } => "Rotate",
            Self::Flip { .. } => "Flip / Mirror",
            Self::BrightnessContrast { .. } => "Brightness, Contrast & Gamma",
            Self::ColorAdjust { .. } => "Colour & White Balance",
            Self::Deinterlace { .. } => "Deinterlace (YADIF / BWDIF)",
            Self::Sharpen { .. } => "Sharpen (Unsharp / CAS)",
            Self::Denoise { .. } => "Denoise (Spatial + Temporal)",
            Self::Grayscale => "Grayscale (Monochrome)",
            Self::Vignette { .. } => "Lens Vignette",
            Self::TemporalSpeed { .. } => "Temporal Speed & Framerate",
            Self::Levels { .. } => "Levels (Input / Gamma / Output)",
            Self::HsvAdjust { .. } => "HSV Adjust (Hue / Sat / Value)",
            Self::Blur { .. } => "Blur / Gaussian Blur",
            Self::Invert { .. } => "Invert (Negative)",
            Self::Threshold { .. } => "Threshold (Binary B&W)",
            Self::EmbossEdge { .. } => "Emboss / Edge Detect (3×3)",
            Self::TwoToOneReduction { .. } => "2:1 Reduction (Half Size)",
            Self::BorderFill { .. } => "Fill / Letterbox Border",
            Self::FieldSwap => "Field Swap (Even/Odd Scanlines)",
            Self::TextOverlay { .. } => "Text / Timecode Overlay",
            Self::SubtitleBurnIn { .. } => "Subtitle Burn-In (Hardsubs)",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Self::Crop {
                left,
                top,
                right,
                bottom,
            } => format!("L:{left} T:{top} R:{right} B:{bottom}"),
            Self::Resize {
                width,
                height,
                algorithm,
                spec_mode,
                scale_percent,
                aspect_mode,
            } => match spec_mode {
                ResizeSpecMode::Percentage => {
                    format!("{scale_percent:.0}% ({})", algorithm.label())
                }
                ResizeSpecMode::Dimensions => {
                    format!("{width}×{height} [{:?}, {}]", aspect_mode, algorithm.label())
                }
            },
            Self::Rotate {
                angle,
                custom_degrees,
                ..
            } => match angle {
                RotationAngle::CustomDegrees => format!("{custom_degrees:+.1}°"),
                _ => angle.label().to_string(),
            },
            Self::Flip {
                horizontal,
                vertical,
            } => match (horizontal, vertical) {
                (true, true) => "Horizontal + Vertical".to_string(),
                (true, false) => "Horizontal".to_string(),
                (false, true) => "Vertical".to_string(),
                (false, false) => "No-op".to_string(),
            },
            Self::BrightnessContrast {
                brightness,
                contrast,
                gamma,
            } => format!("B:{brightness:+.2} C:{contrast:.2} G:{gamma:.2}"),
            Self::ColorAdjust {
                saturation,
                gamma,
                hue_degrees,
                temperature,
                ..
            } => format!(
                "Sat:{saturation:.2} Gam:{gamma:.2} Hue:{hue_degrees:+.0}° Temp:{temperature:+.2}"
            ),
            Self::Deinterlace {
                mode,
                engine,
                parity,
            } => format!("{} | {} | {}", engine.label(), mode.label(), parity.label()),
            Self::Sharpen {
                amount,
                mode,
                radius,
                ..
            } => match mode {
                SharpenMode::UnsharpMask => format!("Unsharp {radius}×{radius}: {amount:.2}"),
                SharpenMode::ContrastAdaptive => format!("CAS: {amount:.2}"),
            },
            Self::Denoise {
                luma_spatial,
                chroma_spatial,
                luma_temporal,
                mode,
                ..
            } => format!(
                "{:?} S:{luma_spatial:.1}/{chroma_spatial:.1} T:{luma_temporal:.1}",
                mode
            ),
            Self::Grayscale => "BT.709 Luma".to_string(),
            Self::Vignette { angle_rad, .. } => format!("Angle: {angle_rad:.2} rad"),
            Self::TemporalSpeed {
                speed_factor,
                target_fps,
            } => match target_fps {
                Some(fps) => format!("{speed_factor:.2}x @ {fps:.2} fps"),
                None => format!("{speed_factor:.2}x speed"),
            },
            Self::Levels {
                input_black,
                input_white,
                gamma,
                output_black,
                output_white,
                ..
            } => format!("In:{input_black}..{input_white} G:{gamma:.2} Out:{output_black}..{output_white}"),
            Self::HsvAdjust {
                hue_degrees,
                saturation,
                value,
            } => format!("H:{hue_degrees:+.0}° S:{saturation:.2} V:{value:.2}"),
            Self::Blur { sigma, steps } => format!("Sigma:{sigma:.1} Steps:{steps}"),
            Self::Invert {
                invert_r,
                invert_g,
                invert_b,
            } => format!(
                "Invert {}{}{}",
                if *invert_r { "R" } else { "" },
                if *invert_g { "G" } else { "" },
                if *invert_b { "B" } else { "" }
            ),
            Self::Threshold { level } => format!("Cutoff: {level}/255"),
            Self::EmbossEdge { preset, strength } => format!("{} ({strength:.1}x)", preset.label()),
            Self::TwoToOneReduction { high_quality } => {
                if *high_quality {
                    "2:1 High Quality (3-tap)".to_string()
                } else {
                    "2:1 Box Average".to_string()
                }
            }
            Self::BorderFill {
                left,
                top,
                right,
                bottom,
                expand_canvas,
                ..
            } => format!(
                "L:{left} T:{top} R:{right} B:{bottom} ({})",
                if *expand_canvas { "Pad" } else { "Mask" }
            ),
            Self::FieldSwap => "Swap Even/Odd Lines".to_string(),
            Self::TextOverlay {
                text,
                show_timecode,
                show_frame_number,
                position,
                ..
            } => format!(
                "{} | TC:{} Frm:{} '{}'",
                position.label(),
                if *show_timecode { "On" } else { "Off" },
                if *show_frame_number { "On" } else { "Off" },
                text
            ),
            Self::SubtitleBurnIn {
                subtitle_path,
                font_size,
            } => {
                if subtitle_path.is_empty() {
                    format!("Embedded Subtitle Track ({font_size}pt)")
                } else {
                    format!("{subtitle_path} ({font_size}pt)")
                }
            }
        }
    }

    /// Converts this single filter into an FFmpeg filtergraph expression, given the current frame dimensions.
    pub fn to_ffmpeg_expr(&self, current_w: u32, current_h: u32) -> Option<(String, u32, u32)> {
        match self {
            Self::Crop {
                left,
                top,
                right,
                bottom,
            } => {
                let out_w = current_w.saturating_sub(left + right).max(2) & !1;
                let out_h = current_h.saturating_sub(top + bottom).max(2) & !1;
                Some((
                    format!("crop={out_w}:{out_h}:{left}:{top}"),
                    out_w,
                    out_h,
                ))
            }
            Self::Resize {
                width,
                height,
                algorithm,
                spec_mode,
                scale_percent,
                aspect_mode,
            } => {
                let flag = algorithm.ffmpeg_flag();
                let (w, h) = Self::compute_resize_dimensions(
                    current_w,
                    current_h,
                    *width,
                    *height,
                    *spec_mode,
                    *scale_percent,
                    *aspect_mode,
                );
                let expr = match (*spec_mode, *aspect_mode) {
                    (ResizeSpecMode::Dimensions, AspectRatioMode::FitAndPad) => {
                        format!(
                            "scale={w}:{h}:force_original_aspect_ratio=decrease:flags={flag},pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black"
                        )
                    }
                    (ResizeSpecMode::Dimensions, AspectRatioMode::FillAndCrop) => {
                        format!(
                            "scale={w}:{h}:force_original_aspect_ratio=increase:flags={flag},crop={w}:{h}"
                        )
                    }
                    _ => format!("scale={w}:{h}:flags={flag}"),
                };
                Some((expr, w, h))
            }
            Self::Rotate {
                angle,
                custom_degrees,
                bilinear,
            } => match angle {
                RotationAngle::Rotate90Cw => {
                    Some(("transpose=1".to_string(), current_h, current_w))
                }
                RotationAngle::Rotate180 => {
                    Some(("hflip,vflip".to_string(), current_w, current_h))
                }
                RotationAngle::Rotate270Cw => {
                    Some(("transpose=2".to_string(), current_h, current_w))
                }
                RotationAngle::CustomDegrees => {
                    if custom_degrees.abs() < 0.01 {
                        None
                    } else {
                        let b = if *bilinear { 1 } else { 0 };
                        Some((
                            format!("rotate={custom_degrees:.3}*PI/180:bilinear={b}"),
                            current_w,
                            current_h,
                        ))
                    }
                }
            },
            Self::Flip {
                horizontal,
                vertical,
            } => {
                let mut parts = Vec::new();
                if *horizontal {
                    parts.push("hflip");
                }
                if *vertical {
                    parts.push("vflip");
                }
                if parts.is_empty() {
                    None
                } else {
                    Some((parts.join(","), current_w, current_h))
                }
            }
            Self::BrightnessContrast {
                brightness,
                contrast,
                gamma,
            } => Some((
                format!("eq=brightness={brightness:.3}:contrast={contrast:.3}:gamma={gamma:.3}"),
                current_w,
                current_h,
            )),
            Self::ColorAdjust {
                saturation,
                gamma,
                hue_degrees,
                temperature,
                r_offset,
                g_offset,
                b_offset,
            } => {
                let mut chain = format!("eq=saturation={saturation:.3}:gamma={gamma:.3}");
                if hue_degrees.abs() > 0.01 {
                    chain.push_str(&format!(",hue=h={hue_degrees:.2}"));
                }
                let rm = (r_offset + temperature * 0.25).clamp(-1.0, 1.0);
                let gm = (*g_offset).clamp(-1.0, 1.0);
                let bm = (b_offset - temperature * 0.25).clamp(-1.0, 1.0);
                if rm.abs() > 0.005 || gm.abs() > 0.005 || bm.abs() > 0.005 {
                    chain.push_str(&format!(",colorbalance=rm={rm:.3}:gm={gm:.3}:bm={bm:.3}"));
                }
                Some((chain, current_w, current_h))
            }
            Self::Deinterlace {
                mode,
                engine,
                parity,
            } => {
                let m = match mode {
                    DeinterlaceMode::SendFrame => 0,
                    DeinterlaceMode::SendField => 1,
                };
                let p = parity.ffmpeg_val();
                let expr = match engine {
                    DeinterlaceEngine::Yadif => format!("yadif=mode={m}:parity={p}"),
                    DeinterlaceEngine::Bwdif => format!("bwdif=mode={m}:parity={p}"),
                };
                Some((expr, current_w, current_h))
            }
            Self::Sharpen {
                amount,
                mode,
                radius,
                chroma_amount,
            } => match mode {
                SharpenMode::UnsharpMask => {
                    let r = (*radius).clamp(3, 7) | 1;
                    Some((
                        format!("unsharp={r}:{r}:{amount:.2}:{r}:{r}:{chroma_amount:.2}"),
                        current_w,
                        current_h,
                    ))
                }
                SharpenMode::ContrastAdaptive => {
                    let strength = (*amount / 3.0).clamp(0.0, 1.0);
                    Some((format!("cas={strength:.3}"), current_w, current_h))
                }
            },
            Self::Denoise {
                luma_spatial,
                chroma_spatial,
                luma_temporal,
                chroma_temporal,
                mode,
            } => match mode {
                DenoiseMode::Hqdn3d => Some((
                    format!(
                        "hqdn3d={luma_spatial:.2}:{chroma_spatial:.2}:{luma_temporal:.2}:{chroma_temporal:.2}"
                    ),
                    current_w,
                    current_h,
                )),
                DenoiseMode::AtaDenoise => Some((
                    "atadenoise=0a=0.02:0b=0.04:1a=0.02:1b=0.04:2a=0.02:2b=0.04".to_string(),
                    current_w,
                    current_h,
                )),
            },
            Self::Grayscale => Some(("hue=s=0".to_string(), current_w, current_h)),
            Self::Vignette {
                angle_rad,
                backward,
            } => {
                let mode_str = if *backward { "backward" } else { "forward" };
                Some((
                    format!("vignette=angle={angle_rad:.3}:mode={mode_str}"),
                    current_w,
                    current_h,
                ))
            }
            Self::TemporalSpeed {
                speed_factor,
                target_fps,
            } => {
                let mut parts = Vec::new();
                let spd = (*speed_factor).clamp(0.1, 8.0);
                if (spd - 1.0).abs() > 0.005 {
                    parts.push(format!("setpts=PTS/{spd:.4}"));
                }
                if let Some(fps) = target_fps {
                    if *fps >= 1.0 {
                        parts.push(format!("fps={fps:.3}"));
                    }
                }
                if parts.is_empty() {
                    None
                } else {
                    Some((parts.join(","), current_w, current_h))
                }
            }
            Self::Levels {
                input_black,
                input_white,
                gamma,
                output_black,
                output_white,
                ..
            } => {
                let ib = (*input_black as f32 / 255.0).clamp(0.0, 0.99);
                let iw = (*input_white as f32 / 255.0).clamp(ib + 0.01, 1.0);
                let ob = (*output_black as f32 / 255.0).clamp(0.0, 1.0);
                let ow = (*output_white as f32 / 255.0).clamp(0.0, 1.0);
                let g = (*gamma).clamp(0.1, 5.0);
                let mut expr = format!(
                    "colorlevels=rimin={ib:.3}:gimin={ib:.3}:bimin={ib:.3}:rimax={iw:.3}:gimax={iw:.3}:bimax={iw:.3}:romin={ob:.3}:gomin={ob:.3}:bomin={ob:.3}:romax={ow:.3}:gomax={ow:.3}:bomax={ow:.3}"
                );
                if (g - 1.0).abs() > 0.01 {
                    expr.push_str(&format!(",eq=gamma={g:.3}"));
                }
                Some((expr, current_w, current_h))
            }
            Self::HsvAdjust {
                hue_degrees,
                saturation,
                value,
            } => {
                let h = (*hue_degrees).clamp(-180.0, 180.0);
                let s = (*saturation).clamp(0.0, 3.0);
                let b = (*value - 1.0).clamp(-1.0, 1.0);
                let mut expr = format!("hue=h={h:.2}:s={s:.3}");
                if b.abs() > 0.005 {
                    expr.push_str(&format!(",eq=brightness={b:.3}"));
                }
                Some((expr, current_w, current_h))
            }
            Self::Blur { sigma, steps } => {
                let sig = (*sigma).clamp(0.1, 30.0);
                let st = (*steps).clamp(1, 6);
                Some((format!("gblur=sigma={sig:.2}:steps={st}"), current_w, current_h))
            }
            Self::Invert {
                invert_r,
                invert_g,
                invert_b,
            } => {
                if *invert_r && *invert_g && *invert_b {
                    Some(("negate".to_string(), current_w, current_h))
                } else if !*invert_r && !*invert_g && !*invert_b {
                    None
                } else {
                    let r_ex = if *invert_r { "maxval-val" } else { "val" };
                    let g_ex = if *invert_g { "maxval-val" } else { "val" };
                    let b_ex = if *invert_b { "maxval-val" } else { "val" };
                    Some((
                        format!("lutrgb=r='{r_ex}':g='{g_ex}':b='{b_ex}'"),
                        current_w,
                        current_h,
                    ))
                }
            }
            Self::Threshold { level } => {
                let lvl = *level;
                Some((
                    format!("format=gray,lut=y='if(gte(val\\,{lvl})\\,255\\,0)',format=yuv420p"),
                    current_w,
                    current_h,
                ))
            }
            Self::EmbossEdge { preset, strength } => {
                let _ = strength;
                let expr = match preset {
                    ConvolutionPreset::Emboss => {
                        "convolution='-2 -1 0 -1 1 1 0 1 2:-2 -1 0 -1 1 1 0 1 2:-2 -1 0 -1 1 1 0 1 2:-2 -1 0 -1 1 1 0 1 2:1:1:1:1:128:128:128:0'".to_string()
                    }
                    ConvolutionPreset::SobelEdge => "sobel".to_string(),
                    ConvolutionPreset::LaplaceOutline => {
                        "convolution='0 -1 0 -1 4 -1 0 -1 0:0 -1 0 -1 4 -1 0 -1 0:0 -1 0 -1 4 -1 0 -1 0:0 -1 0 -1 4 -1 0 -1 0'".to_string()
                    }
                };
                Some((expr, current_w, current_h))
            }
            Self::TwoToOneReduction { high_quality } => {
                let nw = ((current_w / 2).max(2)) & !1;
                let nh = ((current_h / 2).max(2)) & !1;
                let flags = if *high_quality { "lanczos" } else { "bilinear" };
                Some((format!("scale={nw}:{nh}:flags={flags}"), nw, nh))
            }
            Self::BorderFill {
                left,
                top,
                right,
                bottom,
                color_rgb,
                expand_canvas,
            } => {
                let hex = format!(
                    "0x{:02X}{:02X}{:02X}",
                    color_rgb[0], color_rgb[1], color_rgb[2]
                );
                if *expand_canvas {
                    let nw = (current_w + left + right).max(2) & !1;
                    let nh = (current_h + top + bottom).max(2) & !1;
                    Some((
                        format!("pad={nw}:{nh}:{left}:{top}:color={hex}"),
                        nw,
                        nh,
                    ))
                } else {
                    let mut draws = Vec::new();
                    if *top > 0 {
                        draws.push(format!("drawbox=x=0:y=0:w=iw:h={top}:color={hex}:t=fill"));
                    }
                    if *bottom > 0 {
                        draws.push(format!(
                            "drawbox=x=0:y=ih-{bottom}:w=iw:h={bottom}:color={hex}:t=fill"
                        ));
                    }
                    if *left > 0 {
                        draws.push(format!("drawbox=x=0:y=0:w={left}:h=ih:color={hex}:t=fill"));
                    }
                    if *right > 0 {
                        draws.push(format!(
                            "drawbox=x=iw-{right}:w={right}:y=0:h=ih:color={hex}:t=fill"
                        ));
                    }
                    if draws.is_empty() {
                        None
                    } else {
                        Some((draws.join(","), current_w, current_h))
                    }
                }
            }
            Self::FieldSwap => Some((
                "il=l=d:c=d,il=l=i:c=i:ls=1:cs=1".to_string(),
                current_w,
                current_h,
            )),
            Self::TextOverlay {
                text,
                show_timecode,
                show_frame_number,
                position,
                font_size,
                box_opacity,
            } => {
                let mut parts = Vec::new();
                if !text.trim().is_empty() {
                    let sanitized = text
                        .replace('\\', "\\\\")
                        .replace(':', "\\:")
                        .replace('\'', "");
                    parts.push(sanitized);
                }
                if *show_timecode {
                    parts.push("%{pts\\:hms}".to_string());
                }
                if *show_frame_number {
                    parts.push("F\\:%{n}".to_string());
                }
                if parts.is_empty() {
                    return None;
                }
                let combined = parts.join("  ");
                let (x_ex, y_ex) = match position {
                    OverlayPosition::TopLeft => ("24", "24"),
                    OverlayPosition::TopCenter => ("(w-tw)/2", "24"),
                    OverlayPosition::TopRight => ("w-tw-24", "24"),
                    OverlayPosition::BottomLeft => ("24", "h-th-24"),
                    OverlayPosition::BottomCenter => ("(w-tw)/2", "h-th-24"),
                    OverlayPosition::BottomRight => ("w-tw-24", "h-th-24"),
                };
                let sz = (*font_size).clamp(10, 120);
                let bop = (*box_opacity).clamp(0.0, 1.0);
                Some((
                    format!(
                        "drawtext=text='{combined}':fontsize={sz}:fontcolor=white:x={x_ex}:y={y_ex}:box=1:boxcolor=black@{bop:.2}:boxborderw=8"
                    ),
                    current_w,
                    current_h,
                ))
            }
            Self::SubtitleBurnIn {
                subtitle_path,
                font_size,
            } => {
                let trimmed = subtitle_path.trim();
                if trimmed.is_empty() {
                    return None;
                }
                let escaped = trimmed
                    .replace('\\', "\\\\")
                    .replace(':', "\\:")
                    .replace('\'', "\\'");
                let sz = (*font_size).clamp(10, 72);
                Some((
                    format!(
                        "subtitles=filename='{escaped}':force_style='FontSize={sz}'"
                    ),
                    current_w,
                    current_h,
                ))
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FilterInstance {
    pub id: u64,
    pub enabled: bool,
    pub kind: FilterKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FilterPipeline {
    pub filters: Vec<FilterInstance>,
    next_id: u64,
}

impl FilterPipeline {
    pub fn new() -> Self {
        Self {
            filters: Vec::new(),
            next_id: 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        !self.filters.iter().any(|f| f.enabled)
    }

    pub fn add(&mut self, kind: FilterKind) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        self.filters.push(FilterInstance {
            id,
            enabled: true,
            kind,
        });
        id
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.filters.len() {
            self.filters.remove(index);
        }
    }

    pub fn move_up(&mut self, index: usize) {
        if index > 0 && index < self.filters.len() {
            self.filters.swap(index - 1, index);
        }
    }

    pub fn move_down(&mut self, index: usize) {
        if index + 1 < self.filters.len() {
            self.filters.swap(index, index + 1);
        }
    }

    /// Returns the dimensions entering the filter at `target_index` given initial `(source_w, source_h)`.
    pub fn input_dimensions_at(&self, target_index: usize, source_w: u32, source_h: u32) -> (u32, u32) {
        let mut cur_w = source_w;
        let mut cur_h = source_h;
        for (idx, instance) in self.filters.iter().enumerate() {
            if idx >= target_index {
                break;
            }
            if !instance.enabled {
                continue;
            }
            if let Some((_, next_w, next_h)) = instance.kind.to_ffmpeg_expr(cur_w, cur_h) {
                cur_w = next_w;
                cur_h = next_h;
            }
        }
        (cur_w, cur_h)
    }

    /// Computes the FFmpeg `-vf` filtergraph string and the final output dimensions.
    pub fn to_ffmpeg_filtergraph(&self, source_w: u32, source_h: u32) -> (Option<String>, u32, u32) {
        let mut cur_w = source_w;
        let mut cur_h = source_h;
        let mut exprs = Vec::new();

        for instance in &self.filters {
            if !instance.enabled {
                continue;
            }
            if let Some((expr, next_w, next_h)) = instance.kind.to_ffmpeg_expr(cur_w, cur_h) {
                exprs.push(expr);
                cur_w = next_w;
                cur_h = next_h;
            }
        }

        if exprs.is_empty() {
            (None, cur_w, cur_h)
        } else {
            (Some(exprs.join(",")), cur_w, cur_h)
        }
    }

    /// Applies all enabled filters in-memory to an RGBA8 buffer `(width, height, rgba_bytes)`
    /// for real-time interactive Output preview.
    pub fn apply_to_rgba(
        &self,
        mut width: u32,
        mut height: u32,
        mut pixels: Vec<u8>,
    ) -> (u32, u32, Vec<u8>) {
        for instance in &self.filters {
            if !instance.enabled {
                continue;
            }
            match &instance.kind {
                FilterKind::Crop {
                    left,
                    top,
                    right,
                    bottom,
                } => {
                    let out_w = width.saturating_sub(left + right).max(2) & !1;
                    let out_h = height.saturating_sub(top + bottom).max(2) & !1;
                    let l = (*left).min(width.saturating_sub(out_w));
                    let t = (*top).min(height.saturating_sub(out_h));
                    let mut out = vec![0u8; (out_w * out_h * 4) as usize];
                    for y in 0..out_h {
                        let src_row_start = (((t + y) * width + l) * 4) as usize;
                        let dst_row_start = ((y * out_w) * 4) as usize;
                        let row_bytes = (out_w * 4) as usize;
                        if src_row_start + row_bytes <= pixels.len() {
                            out[dst_row_start..dst_row_start + row_bytes]
                                .copy_from_slice(&pixels[src_row_start..src_row_start + row_bytes]);
                        }
                    }
                    width = out_w;
                    height = out_h;
                    pixels = out;
                }
                FilterKind::Resize {
                    width: target_w,
                    height: target_h,
                    spec_mode,
                    scale_percent,
                    aspect_mode,
                    ..
                } => {
                    let (out_w, out_h) = FilterKind::compute_resize_dimensions(
                        width,
                        height,
                        *target_w,
                        *target_h,
                        *spec_mode,
                        *scale_percent,
                        *aspect_mode,
                    );
                    if out_w != width || out_h != height || *aspect_mode != AspectRatioMode::Stretch {
                        let mut out = vec![0u8; (out_w * out_h * 4) as usize];
                        // Initialize alpha channel to 255 (opaque black for FitAndPad bars)
                        for px in out.chunks_exact_mut(4) {
                            px[3] = 255;
                        }

                        match (*spec_mode, *aspect_mode) {
                            (ResizeSpecMode::Dimensions, AspectRatioMode::FitAndPad) => {
                                let scale = (out_w as f32 / width.max(1) as f32)
                                    .min(out_h as f32 / height.max(1) as f32);
                                let inner_w = ((width as f32 * scale).round() as u32).clamp(1, out_w);
                                let inner_h = ((height as f32 * scale).round() as u32).clamp(1, out_h);
                                let pad_x = (out_w - inner_w) / 2;
                                let pad_y = (out_h - inner_h) / 2;
                                for y in 0..inner_h {
                                    let src_y = ((y as u64 * height as u64) / inner_h as u64) as u32;
                                    for x in 0..inner_w {
                                        let src_x = ((x as u64 * width as u64) / inner_w as u64) as u32;
                                        let s_idx = ((src_y * width + src_x) * 4) as usize;
                                        let d_idx = (((y + pad_y) * out_w + (x + pad_x)) * 4) as usize;
                                        out[d_idx..d_idx + 4].copy_from_slice(&pixels[s_idx..s_idx + 4]);
                                    }
                                }
                            }
                            (ResizeSpecMode::Dimensions, AspectRatioMode::FillAndCrop) => {
                                let scale = (out_w as f32 / width.max(1) as f32)
                                    .max(out_h as f32 / height.max(1) as f32);
                                let scaled_w = ((width as f32 * scale).round() as u32).max(out_w);
                                let scaled_h = ((height as f32 * scale).round() as u32).max(out_h);
                                let crop_x = (scaled_w - out_w) / 2;
                                let crop_y = (scaled_h - out_h) / 2;
                                for y in 0..out_h {
                                    let src_y = (((y + crop_y) as u64 * height as u64) / scaled_h as u64)
                                        .min(height.saturating_sub(1) as u64) as u32;
                                    for x in 0..out_w {
                                        let src_x = (((x + crop_x) as u64 * width as u64) / scaled_w as u64)
                                            .min(width.saturating_sub(1) as u64) as u32;
                                        let s_idx = ((src_y * width + src_x) * 4) as usize;
                                        let d_idx = ((y * out_w + x) * 4) as usize;
                                        out[d_idx..d_idx + 4].copy_from_slice(&pixels[s_idx..s_idx + 4]);
                                    }
                                }
                            }
                            _ => {
                                for y in 0..out_h {
                                    let src_y = ((y as u64 * height as u64) / out_h as u64) as u32;
                                    for x in 0..out_w {
                                        let src_x = ((x as u64 * width as u64) / out_w as u64) as u32;
                                        let s_idx = ((src_y * width + src_x) * 4) as usize;
                                        let d_idx = ((y * out_w + x) * 4) as usize;
                                        out[d_idx..d_idx + 4].copy_from_slice(&pixels[s_idx..s_idx + 4]);
                                    }
                                }
                            }
                        }

                        width = out_w;
                        height = out_h;
                        pixels = out;
                    }
                }
                FilterKind::Rotate {
                    angle,
                    custom_degrees,
                    ..
                } => match angle {
                    RotationAngle::Rotate90Cw => {
                        let out_w = height;
                        let out_h = width;
                        let mut out = vec![0u8; (out_w * out_h * 4) as usize];
                        for y in 0..height {
                            for x in 0..width {
                                let dst_x = height - 1 - y;
                                let dst_y = x;
                                let s_idx = ((y * width + x) * 4) as usize;
                                let d_idx = ((dst_y * out_w + dst_x) * 4) as usize;
                                out[d_idx..d_idx + 4].copy_from_slice(&pixels[s_idx..s_idx + 4]);
                            }
                        }
                        width = out_w;
                        height = out_h;
                        pixels = out;
                    }
                    RotationAngle::Rotate180 => {
                        let total_px = (width * height) as usize;
                        for i in 0..(total_px / 2) {
                            let j = total_px - 1 - i;
                            for c in 0..4 {
                                pixels.swap(i * 4 + c, j * 4 + c);
                            }
                        }
                    }
                    RotationAngle::Rotate270Cw => {
                        let out_w = height;
                        let out_h = width;
                        let mut out = vec![0u8; (out_w * out_h * 4) as usize];
                        for y in 0..height {
                            for x in 0..width {
                                let dst_x = y;
                                let dst_y = width - 1 - x;
                                let s_idx = ((y * width + x) * 4) as usize;
                                let d_idx = ((dst_y * out_w + dst_x) * 4) as usize;
                                out[d_idx..d_idx + 4].copy_from_slice(&pixels[s_idx..s_idx + 4]);
                            }
                        }
                        width = out_w;
                        height = out_h;
                        pixels = out;
                    }
                    RotationAngle::CustomDegrees => {
                        if custom_degrees.abs() >= 0.05 {
                            let rad = -custom_degrees.to_radians();
                            let cos_a = rad.cos();
                            let sin_a = rad.sin();
                            let cx = width as f32 * 0.5;
                            let cy = height as f32 * 0.5;
                            let mut out = vec![0u8; (width * height * 4) as usize];
                            for px in out.chunks_exact_mut(4) {
                                px[3] = 255;
                            }
                            for y in 0..height {
                                let dy = y as f32 - cy;
                                for x in 0..width {
                                    let dx = x as f32 - cx;
                                    let sx = (dx * cos_a - dy * sin_a + cx).round() as i32;
                                    let sy = (dx * sin_a + dy * cos_a + cy).round() as i32;
                                    if sx >= 0 && sx < width as i32 && sy >= 0 && sy < height as i32
                                    {
                                        let s_idx = ((sy as u32 * width + sx as u32) * 4) as usize;
                                        let d_idx = ((y * width + x) * 4) as usize;
                                        out[d_idx..d_idx + 4]
                                            .copy_from_slice(&pixels[s_idx..s_idx + 4]);
                                    }
                                }
                            }
                            pixels = out;
                        }
                    }
                },
                FilterKind::Flip {
                    horizontal,
                    vertical,
                } => {
                    if *horizontal {
                        for y in 0..height {
                            for x in 0..(width / 2) {
                                let a = ((y * width + x) * 4) as usize;
                                let b = ((y * width + (width - 1 - x)) * 4) as usize;
                                for c in 0..4 {
                                    pixels.swap(a + c, b + c);
                                }
                            }
                        }
                    }
                    if *vertical {
                        let row_bytes = (width * 4) as usize;
                        for y in 0..(height / 2) {
                            let a = (y * width * 4) as usize;
                            let b = ((height - 1 - y) * width * 4) as usize;
                            for i in 0..row_bytes {
                                pixels.swap(a + i, b + i);
                            }
                        }
                    }
                }
                FilterKind::BrightnessContrast {
                    brightness,
                    contrast,
                    gamma,
                } => {
                    let b_offset = brightness * 255.0;
                    let c = *contrast;
                    let inv_gamma = 1.0 / gamma.max(0.1);
                    let mut lut = [0u8; 256];
                    for (i, slot) in lut.iter_mut().enumerate() {
                        let v = ((i as f32 - 128.0) * c + 128.0 + b_offset).clamp(0.0, 255.0);
                        let g_norm = (v / 255.0).powf(inv_gamma);
                        *slot = (g_norm * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                    for px in pixels.chunks_exact_mut(4) {
                        px[0] = lut[px[0] as usize];
                        px[1] = lut[px[1] as usize];
                        px[2] = lut[px[2] as usize];
                    }
                }
                FilterKind::ColorAdjust {
                    saturation,
                    gamma,
                    hue_degrees,
                    temperature,
                    r_offset,
                    g_offset,
                    b_offset,
                } => {
                    let inv_gamma = 1.0 / gamma.max(0.1);
                    let mut gamma_lut = [0u8; 256];
                    for (i, slot) in gamma_lut.iter_mut().enumerate() {
                        let norm = (i as f32 / 255.0).powf(inv_gamma);
                        *slot = (norm * 255.0).round().clamp(0.0, 255.0) as u8;
                    }
                    let sat = *saturation;
                    let hue_rad = hue_degrees.to_radians();
                    let cos_h = hue_rad.cos();
                    let sin_h = hue_rad.sin();
                    let r_shift = (r_offset + temperature * 0.25) * 100.0;
                    let g_shift = g_offset * 100.0;
                    let b_shift = (b_offset - temperature * 0.25) * 100.0;

                    for px in pixels.chunks_exact_mut(4) {
                        let r = (gamma_lut[px[0] as usize] as f32 + r_shift).clamp(0.0, 255.0);
                        let g = (gamma_lut[px[1] as usize] as f32 + g_shift).clamp(0.0, 255.0);
                        let b = (gamma_lut[px[2] as usize] as f32 + b_shift).clamp(0.0, 255.0);

                        let luma = 0.299 * r + 0.587 * g + 0.114 * b;
                        let mut sr = luma + (r - luma) * sat;
                        let mut sg = luma + (g - luma) * sat;
                        let mut sb = luma + (b - luma) * sat;

                        if hue_degrees.abs() > 0.01 {
                            let u = -0.14713 * sr - 0.28886 * sg + 0.436 * sb;
                            let v = 0.615 * sr - 0.51499 * sg - 0.10001 * sb;
                            let u2 = u * cos_h - v * sin_h;
                            let v2 = u * sin_h + v * cos_h;
                            sr = luma + 1.13983 * v2;
                            sg = luma - 0.39465 * u2 - 0.58060 * v2;
                            sb = luma + 2.03211 * u2;
                        }

                        px[0] = sr.clamp(0.0, 255.0) as u8;
                        px[1] = sg.clamp(0.0, 255.0) as u8;
                        px[2] = sb.clamp(0.0, 255.0) as u8;
                    }
                }
                FilterKind::Deinterlace { .. } => {
                    if height >= 4 {
                        let stride = (width * 4) as usize;
                        let copy = pixels.clone();
                        for y in (1..height as usize - 1).step_by(2) {
                            let prev = (y - 1) * stride;
                            let curr = y * stride;
                            let next = (y + 1) * stride;
                            for i in 0..stride {
                                pixels[curr + i] = ((copy[prev + i] as u16
                                    + copy[curr + i] as u16 * 2
                                    + copy[next + i] as u16)
                                    / 4) as u8;
                            }
                        }
                    }
                }
                FilterKind::Sharpen { amount, .. } => {
                    if width >= 3 && height >= 3 && *amount > 0.01 {
                        let copy = pixels.clone();
                        let w = width as usize;
                        let h = height as usize;
                        let a = *amount;
                        for y in 1..(h - 1) {
                            for x in 1..(w - 1) {
                                let idx = (y * w + x) * 4;
                                let top = ((y - 1) * w + x) * 4;
                                let bot = ((y + 1) * w + x) * 4;
                                let lft = (y * w + (x - 1)) * 4;
                                let rgt = (y * w + (x + 1)) * 4;
                                for c in 0..3 {
                                    let center = copy[idx + c] as f32;
                                    let avg = (copy[top + c] as f32
                                        + copy[bot + c] as f32
                                        + copy[lft + c] as f32
                                        + copy[rgt + c] as f32)
                                        * 0.25;
                                    let val = center + (center - avg) * a;
                                    pixels[idx + c] = val.clamp(0.0, 255.0) as u8;
                                }
                            }
                        }
                    }
                }
                FilterKind::Denoise { luma_spatial, .. } => {
                    if width >= 3 && height >= 3 && *luma_spatial > 0.1 {
                        let copy = pixels.clone();
                        let w = width as usize;
                        let h = height as usize;
                        let blend = (*luma_spatial / 16.0).clamp(0.05, 0.85);
                        for y in 1..(h - 1) {
                            for x in 1..(w - 1) {
                                let idx = (y * w + x) * 4;
                                let top = ((y - 1) * w + x) * 4;
                                let bot = ((y + 1) * w + x) * 4;
                                let lft = (y * w + (x - 1)) * 4;
                                let rgt = (y * w + (x + 1)) * 4;
                                for c in 0..3 {
                                    let center = copy[idx + c] as f32;
                                    let avg = (copy[top + c] as f32
                                        + copy[bot + c] as f32
                                        + copy[lft + c] as f32
                                        + copy[rgt + c] as f32
                                        + center)
                                        * 0.2;
                                    pixels[idx + c] =
                                        (center * (1.0 - blend) + avg * blend).clamp(0.0, 255.0)
                                            as u8;
                                }
                            }
                        }
                    }
                }
                FilterKind::Grayscale => {
                    for px in pixels.chunks_exact_mut(4) {
                        let luma = (0.2126 * px[0] as f32
                            + 0.7152 * px[1] as f32
                            + 0.0722 * px[2] as f32)
                            .clamp(0.0, 255.0) as u8;
                        px[0] = luma;
                        px[1] = luma;
                        px[2] = luma;
                    }
                }
                FilterKind::Vignette {
                    angle_rad,
                    backward,
                } => {
                    let cx = width as f32 * 0.5;
                    let cy = height as f32 * 0.5;
                    let max_d = (cx * cx + cy * cy).sqrt().max(1.0);
                    let strength = (*angle_rad).clamp(0.05, 1.5);
                    for y in 0..height {
                        let dy = y as f32 - cy;
                        for x in 0..width {
                            let dx = x as f32 - cx;
                            let r_norm = (dx * dx + dy * dy).sqrt() / max_d;
                            let falloff = (r_norm * strength).cos().powi(4);
                            let factor = if *backward {
                                (2.0 - falloff).clamp(1.0, 2.0)
                            } else {
                                falloff.clamp(0.1, 1.0)
                            };
                            let idx = ((y * width + x) * 4) as usize;
                            for c in 0..3 {
                                pixels[idx + c] =
                                    (pixels[idx + c] as f32 * factor).clamp(0.0, 255.0) as u8;
                            }
                        }
                    }
                }
                FilterKind::TemporalSpeed { .. } => {
                    // Temporal filter affects frame timing/PTS during export; single-frame RGBA is unchanged
                }
                FilterKind::Levels {
                    input_black,
                    input_white,
                    gamma,
                    output_black,
                    output_white,
                    luma_only,
                } => {
                    let ib = *input_black as f32;
                    let iw = (*input_white as f32).max(ib + 1.0);
                    let ob = *output_black as f32;
                    let ow = *output_white as f32;
                    let inv_gamma = 1.0 / (*gamma).clamp(0.1, 5.0);
                    let mut lut = [0u8; 256];
                    for (i, slot) in lut.iter_mut().enumerate() {
                        let norm = ((i as f32 - ib) / (iw - ib)).clamp(0.0, 1.0);
                        let g_val = norm.powf(inv_gamma);
                        let out = ob + g_val * (ow - ob);
                        *slot = out.round().clamp(0.0, 255.0) as u8;
                    }
                    if *luma_only {
                        for px in pixels.chunks_exact_mut(4) {
                            let y = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
                            let y_idx = y.round().clamp(0.0, 255.0) as usize;
                            let new_y = lut[y_idx] as f32;
                            let scale = if y > 1.0 { new_y / y } else { 1.0 };
                            px[0] = (px[0] as f32 * scale).clamp(0.0, 255.0) as u8;
                            px[1] = (px[1] as f32 * scale).clamp(0.0, 255.0) as u8;
                            px[2] = (px[2] as f32 * scale).clamp(0.0, 255.0) as u8;
                        }
                    } else {
                        for px in pixels.chunks_exact_mut(4) {
                            px[0] = lut[px[0] as usize];
                            px[1] = lut[px[1] as usize];
                            px[2] = lut[px[2] as usize];
                        }
                    }
                }
                FilterKind::HsvAdjust {
                    hue_degrees,
                    saturation,
                    value,
                } => {
                    let hue_rad = hue_degrees.to_radians();
                    let cos_h = hue_rad.cos();
                    let sin_h = hue_rad.sin();
                    let sat = (*saturation).clamp(0.0, 3.0);
                    let val = (*value).clamp(0.0, 3.0);
                    for px in pixels.chunks_exact_mut(4) {
                        let r = (px[0] as f32 / 255.0) * val;
                        let g = (px[1] as f32 / 255.0) * val;
                        let b = (px[2] as f32 / 255.0) * val;
                        let y = 0.299 * r + 0.587 * g + 0.114 * b;
                        let i = 0.596 * r - 0.274 * g - 0.322 * b;
                        let q = 0.211 * r - 0.523 * g + 0.312 * b;
                        let i_rot = (i * cos_h - q * sin_h) * sat;
                        let q_rot = (i * sin_h + q * cos_h) * sat;
                        px[0] = ((y + 0.956 * i_rot + 0.621 * q_rot) * 255.0).clamp(0.0, 255.0) as u8;
                        px[1] = ((y - 0.272 * i_rot - 0.647 * q_rot) * 255.0).clamp(0.0, 255.0) as u8;
                        px[2] = ((y - 1.106 * i_rot + 1.703 * q_rot) * 255.0).clamp(0.0, 255.0) as u8;
                    }
                }
                FilterKind::Blur { sigma, steps } => {
                    if width >= 3 && height >= 3 {
                        let radius = (sigma.round() as i32).clamp(1, 8);
                        let passes = (*steps).clamp(1, 3);
                        let w = width as i32;
                        let h = height as i32;
                        for _ in 0..passes {
                            let mut horiz = pixels.clone();
                            for y in 0..h {
                                for x in 0..w {
                                    let mut sr = 0u32;
                                    let mut sg = 0u32;
                                    let mut sb = 0u32;
                                    let mut count = 0u32;
                                    for dx in -radius..=radius {
                                        let nx = (x + dx).clamp(0, w - 1);
                                        let idx = ((y * w + nx) * 4) as usize;
                                        sr += pixels[idx] as u32;
                                        sg += pixels[idx + 1] as u32;
                                        sb += pixels[idx + 2] as u32;
                                        count += 1;
                                    }
                                    let dst = ((y * w + x) * 4) as usize;
                                    horiz[dst] = (sr / count) as u8;
                                    horiz[dst + 1] = (sg / count) as u8;
                                    horiz[dst + 2] = (sb / count) as u8;
                                }
                            }
                            for y in 0..h {
                                for x in 0..w {
                                    let mut sr = 0u32;
                                    let mut sg = 0u32;
                                    let mut sb = 0u32;
                                    let mut count = 0u32;
                                    for dy in -radius..=radius {
                                        let ny = (y + dy).clamp(0, h - 1);
                                        let idx = ((ny * w + x) * 4) as usize;
                                        sr += horiz[idx] as u32;
                                        sg += horiz[idx + 1] as u32;
                                        sb += horiz[idx + 2] as u32;
                                        count += 1;
                                    }
                                    let dst = ((y * w + x) * 4) as usize;
                                    pixels[dst] = (sr / count) as u8;
                                    pixels[dst + 1] = (sg / count) as u8;
                                    pixels[dst + 2] = (sb / count) as u8;
                                }
                            }
                        }
                    }
                }
                FilterKind::Invert {
                    invert_r,
                    invert_g,
                    invert_b,
                } => {
                    for px in pixels.chunks_exact_mut(4) {
                        if *invert_r {
                            px[0] = 255 - px[0];
                        }
                        if *invert_g {
                            px[1] = 255 - px[1];
                        }
                        if *invert_b {
                            px[2] = 255 - px[2];
                        }
                    }
                }
                FilterKind::Threshold { level } => {
                    let thresh = *level as f32;
                    for px in pixels.chunks_exact_mut(4) {
                        let y = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
                        let v = if y >= thresh { 255u8 } else { 0u8 };
                        px[0] = v;
                        px[1] = v;
                        px[2] = v;
                    }
                }
                FilterKind::EmbossEdge { preset, strength } => {
                    if width >= 3 && height >= 3 {
                        let src = pixels.clone();
                        let w = width as usize;
                        let h = height as usize;
                        let str_val = (*strength).clamp(0.1, 4.0);
                        let (kernel, bias): ([f32; 9], f32) = match preset {
                            ConvolutionPreset::Emboss => ([-2.0, -1.0, 0.0, -1.0, 1.0, 1.0, 0.0, 1.0, 2.0], 128.0),
                            ConvolutionPreset::SobelEdge => ([-1.0, -2.0, -1.0, 0.0, 0.0, 0.0, 1.0, 2.0, 1.0], 64.0),
                            ConvolutionPreset::LaplaceOutline => ([0.0, -1.0, 0.0, -1.0, 4.0, -1.0, 0.0, -1.0, 0.0], 0.0),
                        };
                        for y in 1..(h - 1) {
                            for x in 1..(w - 1) {
                                let dst = (y * w + x) * 4;
                                for c in 0..3 {
                                    let mut sum = 0.0f32;
                                    let mut k = 0;
                                    for ky in 0..3 {
                                        for kx in 0..3 {
                                            let p = ((y + ky - 1) * w + (x + kx - 1)) * 4 + c;
                                            sum += src[p] as f32 * kernel[k];
                                            k += 1;
                                        }
                                    }
                                    pixels[dst + c] = (sum * str_val + bias).clamp(0.0, 255.0) as u8;
                                }
                            }
                        }
                    }
                }
                FilterKind::TwoToOneReduction { .. } => {
                    let nw = ((width / 2).max(2)) & !1;
                    let nh = ((height / 2).max(2)) & !1;
                    let mut dst = vec![255u8; (nw * nh * 4) as usize];
                    for y in 0..nh {
                        for x in 0..nw {
                            let sx0 = (x * 2).min(width - 1);
                            let sx1 = (x * 2 + 1).min(width - 1);
                            let sy0 = (y * 2).min(height - 1);
                            let sy1 = (y * 2 + 1).min(height - 1);
                            let i00 = ((sy0 * width + sx0) * 4) as usize;
                            let i10 = ((sy0 * width + sx1) * 4) as usize;
                            let i01 = ((sy1 * width + sx0) * 4) as usize;
                            let i11 = ((sy1 * width + sx1) * 4) as usize;
                            let d_idx = ((y * nw + x) * 4) as usize;
                            for c in 0..3 {
                                let avg = (pixels[i00 + c] as u32
                                    + pixels[i10 + c] as u32
                                    + pixels[i01 + c] as u32
                                    + pixels[i11 + c] as u32
                                    + 2)
                                    / 4;
                                dst[d_idx + c] = avg as u8;
                            }
                        }
                    }
                    width = nw;
                    height = nh;
                    pixels = dst;
                }
                FilterKind::BorderFill {
                    left,
                    top,
                    right,
                    bottom,
                    color_rgb,
                    expand_canvas,
                } => {
                    if *expand_canvas {
                        let nw = (width + left + right).max(2) & !1;
                        let nh = (height + top + bottom).max(2) & !1;
                        let mut dst = vec![255u8; (nw * nh * 4) as usize];
                        for px in dst.chunks_exact_mut(4) {
                            px[0] = color_rgb[0];
                            px[1] = color_rgb[1];
                            px[2] = color_rgb[2];
                            px[3] = 255;
                        }
                        for y in 0..height {
                            let dy = y + top;
                            if dy >= nh {
                                break;
                            }
                            for x in 0..width {
                                let dx = x + left;
                                if dx >= nw {
                                    break;
                                }
                                let s_idx = ((y * width + x) * 4) as usize;
                                let d_idx = ((dy * nw + dx) * 4) as usize;
                                dst[d_idx..d_idx + 4].copy_from_slice(&pixels[s_idx..s_idx + 4]);
                            }
                        }
                        width = nw;
                        height = nh;
                        pixels = dst;
                    } else {
                        for y in 0..height {
                            for x in 0..width {
                                if x < *left
                                    || x >= width.saturating_sub(*right)
                                    || y < *top
                                    || y >= height.saturating_sub(*bottom)
                                {
                                    let idx = ((y * width + x) * 4) as usize;
                                    pixels[idx] = color_rgb[0];
                                    pixels[idx + 1] = color_rgb[1];
                                    pixels[idx + 2] = color_rgb[2];
                                }
                            }
                        }
                    }
                }
                FilterKind::FieldSwap => {
                    let row_bytes = (width * 4) as usize;
                    for y in (0..(height as usize).saturating_sub(1)).step_by(2) {
                        let r0 = y * row_bytes;
                        let r1 = (y + 1) * row_bytes;
                        for b in 0..row_bytes {
                            pixels.swap(r0 + b, r1 + b);
                        }
                    }
                }
                FilterKind::TextOverlay {
                    text,
                    show_timecode,
                    show_frame_number,
                    position,
                    font_size,
                    box_opacity,
                } => {
                    let mut parts = Vec::new();
                    if !text.trim().is_empty() {
                        parts.push(text.trim().to_string());
                    }
                    if *show_timecode {
                        parts.push("00:00:00.000".to_string());
                    }
                    if *show_frame_number {
                        parts.push("F:0000".to_string());
                    }
                    if !parts.is_empty() {
                        let banner = parts.join("  ");
                        draw_bitmap_banner(
                            &mut pixels,
                            width,
                            height,
                            &banner,
                            *position,
                            *font_size,
                            *box_opacity,
                        );
                    }
                }
                FilterKind::SubtitleBurnIn {
                    subtitle_path,
                    font_size,
                } => {
                    if !subtitle_path.trim().is_empty() {
                        let file_name = std::path::Path::new(subtitle_path.trim())
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("Subtitles");
                        let label = format!("SUB: {file_name}");
                        draw_bitmap_banner(
                            &mut pixels,
                            width,
                            height,
                            &label,
                            OverlayPosition::BottomCenter,
                            *font_size,
                            0.6,
                        );
                    }
                }
            }
        }
        (width, height, pixels)
    }

    /// Returns the combined temporal speed multiplier across all enabled `TemporalSpeed` filters.
    pub fn effective_speed_factor(&self) -> f64 {
        let mut factor = 1.0f64;
        for instance in &self.filters {
            if !instance.enabled {
                continue;
            }
            if let FilterKind::TemporalSpeed { speed_factor, .. } = instance.kind {
                factor *= (speed_factor as f64).clamp(0.1, 8.0);
            }
        }
        factor.clamp(0.1, 8.0)
    }
}

fn glyph_5x7(ch: char) -> [u8; 7] {
    match ch.to_ascii_uppercase() {
        '0' => [0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110],
        '1' => [0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        '2' => [0b01110, 0b10001, 0b00001, 0b00110, 0b01000, 0b10000, 0b11111],
        '3' => [0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110],
        '4' => [0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010],
        '5' => [0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110],
        '6' => [0b00110, 0b01000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110],
        '7' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000],
        '8' => [0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110],
        '9' => [0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00010, 0b01100],
        'A' => [0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'B' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'D' => [0b11100, 0b10010, 0b10001, 0b10001, 0b10001, 0b10010, 0b11100],
        'E' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111],
        'F' => [0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000],
        'G' => [0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01111],
        'H' => [0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001],
        'I' => [0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110],
        'J' => [0b00111, 0b00010, 0b00010, 0b00010, 0b10010, 0b10010, 0b01100],
        'K' => [0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001],
        'L' => [0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111],
        'M' => [0b10001, 0b11011, 0b10101, 0b10101, 0b10001, 0b10001, 0b10001],
        'N' => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
        'O' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'P' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000],
        'Q' => [0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101],
        'R' => [0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001],
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'T' => [0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100],
        'U' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110],
        'V' => [0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100],
        'W' => [0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b11011, 0b10001],
        'X' => [0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001],
        'Y' => [0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100],
        'Z' => [0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111],
        ':' => [0b00000, 0b00100, 0b00100, 0b00000, 0b00100, 0b00100, 0b00000],
        '.' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00110, 0b00110],
        '-' => [0b00000, 0b00000, 0b00000, 0b11111, 0b00000, 0b00000, 0b00000],
        '_' => [0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b00000, 0b11111],
        '/' => [0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000],
        ' ' => [0; 7],
        _ => [0b01110, 0b10001, 0b00010, 0b00100, 0b00100, 0b00000, 0b00100],
    }
}

fn draw_bitmap_banner(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    text: &str,
    position: OverlayPosition,
    font_size: u32,
    box_opacity: f32,
) {
    if width < 32 || height < 32 || text.is_empty() {
        return;
    }
    let scale = (font_size / 8).clamp(1, 6);
    let char_w = 6 * scale;
    let char_h = 7 * scale;
    let pad = 6 * scale;
    let chars: Vec<char> = text.chars().take(48).collect();
    let text_w = (chars.len() as u32) * char_w;
    let box_w = (text_w + pad * 2).min(width);
    let box_h = (char_h + pad * 2).min(height);

    let margin = (16.min(width / 8)).min(height / 8);
    let (bx, by) = match position {
        OverlayPosition::TopLeft => (margin, margin),
        OverlayPosition::TopCenter => (width.saturating_sub(box_w) / 2, margin),
        OverlayPosition::TopRight => (width.saturating_sub(box_w + margin), margin),
        OverlayPosition::BottomLeft => (margin, height.saturating_sub(box_h + margin)),
        OverlayPosition::BottomCenter => (
            width.saturating_sub(box_w) / 2,
            height.saturating_sub(box_h + margin),
        ),
        OverlayPosition::BottomRight => (
            width.saturating_sub(box_w + margin),
            height.saturating_sub(box_h + margin),
        ),
    };

    let alpha = box_opacity.clamp(0.0, 1.0);
    let keep = 1.0 - alpha;
    for y in by..(by + box_h).min(height) {
        for x in bx..(bx + box_w).min(width) {
            let idx = ((y * width + x) * 4) as usize;
            pixels[idx] = (pixels[idx] as f32 * keep) as u8;
            pixels[idx + 1] = (pixels[idx + 1] as f32 * keep) as u8;
            pixels[idx + 2] = (pixels[idx + 2] as f32 * keep) as u8;
        }
    }

    let tx0 = bx + pad;
    let ty0 = by + pad;
    for (ci, ch) in chars.iter().enumerate() {
        let rows = glyph_5x7(*ch);
        let cx = tx0 + (ci as u32) * char_w;
        for (ry, row_bits) in rows.iter().enumerate() {
            for rx in 0..5u32 {
                if (row_bits >> (4 - rx)) & 1 == 1 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let px = cx + rx * scale + sx;
                            let py = ty0 + (ry as u32) * scale + sy;
                            if px < width && py < height {
                                let idx = ((py * width + px) * 4) as usize;
                                pixels[idx] = 255;
                                pixels[idx + 1] = 255;
                                pixels[idx + 2] = 255;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 64-bin normalized RGB and BT.709 Luma histogram + black/white clipping metrics for video scopes.
#[derive(Debug, Clone)]
pub struct FrameHistogram {
    pub r_bins: [f32; 64],
    pub g_bins: [f32; 64],
    pub b_bins: [f32; 64],
    pub luma_bins: [f32; 64],
    pub shadow_clip_pct: f32,
    pub highlight_clip_pct: f32,
    pub mean_luma: f32,
}

impl Default for FrameHistogram {
    fn default() -> Self {
        Self {
            r_bins: [0.0; 64],
            g_bins: [0.0; 64],
            b_bins: [0.0; 64],
            luma_bins: [0.0; 64],
            shadow_clip_pct: 0.0,
            highlight_clip_pct: 0.0,
            mean_luma: 0.0,
        }
    }
}

/// Computes a fast 64-bin RGB + Luma histogram from an RGBA8 frame buffer (subsampling large frames for 0ms UI cost).
pub fn compute_rgba_histogram(rgba: &[u8]) -> FrameHistogram {
    let total_pixels = rgba.len() / 4;
    if total_pixels == 0 {
        return FrameHistogram::default();
    }

    let step = (total_pixels / 16_384).max(1);
    let mut r_counts = [0u32; 64];
    let mut g_counts = [0u32; 64];
    let mut b_counts = [0u32; 64];
    let mut y_counts = [0u32; 64];
    let mut shadows = 0u32;
    let mut highlights = 0u32;
    let mut sum_y = 0u64;
    let mut sampled = 0u32;

    let mut px_idx = 0usize;
    while px_idx < total_pixels {
        let base = px_idx * 4;
        let r = rgba[base] as u32;
        let g = rgba[base + 1] as u32;
        let b = rgba[base + 2] as u32;
        let y = ((54 * r + 183 * g + 19 * b) >> 8).min(255);

        r_counts[(r >> 2) as usize] += 1;
        g_counts[(g >> 2) as usize] += 1;
        b_counts[(b >> 2) as usize] += 1;
        y_counts[(y >> 2) as usize] += 1;

        if y <= 2 {
            shadows += 1;
        } else if y >= 253 {
            highlights += 1;
        }
        sum_y += y as u64;
        sampled += 1;
        px_idx += step;
    }

    let max_bin = (*r_counts.iter().max().unwrap_or(&1))
        .max(*g_counts.iter().max().unwrap_or(&1))
        .max(*b_counts.iter().max().unwrap_or(&1))
        .max(*y_counts.iter().max().unwrap_or(&1))
        .max(1) as f32;

    let mut hist = FrameHistogram::default();
    for i in 0..64 {
        hist.r_bins[i] = (r_counts[i] as f32 / max_bin).clamp(0.0, 1.0);
        hist.g_bins[i] = (g_counts[i] as f32 / max_bin).clamp(0.0, 1.0);
        hist.b_bins[i] = (b_counts[i] as f32 / max_bin).clamp(0.0, 1.0);
        hist.luma_bins[i] = (y_counts[i] as f32 / max_bin).clamp(0.0, 1.0);
    }

    let denom = sampled.max(1) as f32;
    hist.shadow_clip_pct = (shadows as f32 / denom) * 100.0;
    hist.highlight_clip_pct = (highlights as f32 / denom) * 100.0;
    hist.mean_luma = (sum_y as f32 / denom) / 255.0;
    hist
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_pipeline_to_ffmpeg_and_reorder() {
        let mut pipe = FilterPipeline::new();
        pipe.add(FilterKind::Crop {
            left: 100,
            top: 50,
            right: 100,
            bottom: 50,
        });
        pipe.add(FilterKind::Resize {
            width: 1280,
            height: 720,
            algorithm: ScalingAlgorithm::Lanczos,
            spec_mode: ResizeSpecMode::Dimensions,
            scale_percent: 100.0,
            aspect_mode: AspectRatioMode::Stretch,
        });
        pipe.add(FilterKind::Flip {
            horizontal: true,
            vertical: false,
        });

        let (graph, out_w, out_h) = pipe.to_ffmpeg_filtergraph(1920, 1080);
        assert_eq!(out_w, 1280);
        assert_eq!(out_h, 720);
        assert_eq!(
            graph.as_deref(),
            Some("crop=1720:980:100:50,scale=1280:720:flags=lanczos,hflip")
        );

        // Move flip before resize
        pipe.move_up(2);
        let (graph2, _, _) = pipe.to_ffmpeg_filtergraph(1920, 1080);
        assert_eq!(
            graph2.as_deref(),
            Some("crop=1720:980:100:50,hflip,scale=1280:720:flags=lanczos")
        );
    }

    #[test]
    fn test_resize_percentage_and_aspect_modes() {
        let (w, h) = FilterKind::compute_resize_dimensions(
            1920,
            1080,
            1920,
            1080,
            ResizeSpecMode::Percentage,
            50.0,
            AspectRatioMode::KeepAspect,
        );
        assert_eq!((w, h), (960, 540));

        // Fit 1920x1080 inside 1280x1280 box while keeping aspect ratio -> 1280x720
        let (w2, h2) = FilterKind::compute_resize_dimensions(
            1920,
            1080,
            1280,
            1280,
            ResizeSpecMode::Dimensions,
            100.0,
            AspectRatioMode::KeepAspect,
        );
        assert_eq!((w2, h2), (1280, 720));
    }

    #[test]
    fn test_apply_rgba_crop_and_rotate() {
        let mut pipe = FilterPipeline::new();
        pipe.add(FilterKind::Rotate {
            angle: RotationAngle::Rotate90Cw,
            custom_degrees: 0.0,
            bilinear: true,
        });
        let pixels = vec![255u8; 4 * 2 * 4]; // 4x2 image
        let (w, h, out) = pipe.apply_to_rgba(4, 2, pixels);
        assert_eq!(w, 2);
        assert_eq!(h, 4);
        assert_eq!(out.len(), 2 * 4 * 4);
    }
}
