// Copyright (C) 2026 gst-plugins-rs contributors
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use atomic_refcell::AtomicRefCell;
use gst::{glib, subclass::prelude::*};
use gst_base::subclass::prelude::*;
use gst_video::{VideoFormat, prelude::*, subclass::prelude::*};

use std::collections::VecDeque;
use std::sync::LazyLock;
use std::sync::Mutex;

use super::FrameStackMode;

const DEFAULT_NUM_FRAMES: u32 = 8;
const MIN_NUM_FRAMES: u32 = 1;
const MAX_NUM_FRAMES: u32 = 256;
const DEFAULT_MODE: FrameStackMode = FrameStackMode::Lighten;
const DEFAULT_PASSTHROUGH_ALPHA: bool = true;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "framestack",
        gst::DebugColorFlags::empty(),
        Some("Temporal frame accumulator (light HDR / long exposure)"),
    )
});

#[derive(Debug, Clone, Copy)]
struct Settings {
    num_frames: u32,
    mode: FrameStackMode,
    passthrough_alpha: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            num_frames: DEFAULT_NUM_FRAMES,
            mode: DEFAULT_MODE,
            passthrough_alpha: DEFAULT_PASSTHROUGH_ALPHA,
        }
    }
}

/// Position of the alpha byte within a 4-byte pixel for the formats we accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AlphaPos {
    /// No alpha channel (RGB / BGR / Rgbx / Bgrx / Xrgb / Xbgr).
    None,
    /// Alpha is the 4th byte (RGBA / BGRA).
    Last,
    /// Alpha is the 1st byte (ARGB / ABGR).
    First,
}

struct State {
    info: gst_video::VideoInfo,
    /// Bytes per pixel (1..=4 for the formats we accept).
    pixel_stride: usize,
    /// Visible width in bytes (= width * pixel_stride).
    line_bytes: usize,
    /// Source plane stride (with padding) in bytes.
    src_stride: usize,
    /// Position of alpha (if any) within a pixel for passthrough handling.
    alpha_pos: AlphaPos,
    /// Ring buffer of compacted (no padding) frame copies, oldest at front,
    /// newest at back. Each entry has length `line_bytes * height`.
    history: VecDeque<Box<[u8]>>,
}

#[derive(Default)]
pub struct FrameStack {
    settings: Mutex<Settings>,
    state: AtomicRefCell<Option<State>>,
}

impl FrameStack {
    /// Push the current input frame into the history (compact, no row
    /// padding), evicting old entries until `history.len() <= num_frames`.
    fn push_frame(state: &mut State, src: &[u8], num_frames: usize) {
        let height = state.info.height() as usize;
        let mut compact = Vec::with_capacity(state.line_bytes * height);

        if state.src_stride == state.line_bytes {
            // No padding: copy in one go.
            compact.extend_from_slice(&src[..state.line_bytes * height]);
        } else {
            for line in src.chunks_exact(state.src_stride).take(height) {
                compact.extend_from_slice(&line[..state.line_bytes]);
            }
        }

        state.history.push_back(compact.into_boxed_slice());
        while state.history.len() > num_frames {
            state.history.pop_front();
        }
    }

    /// Combine the ring buffer into the in-place output frame according to
    /// `mode`. `dst` is the output plane data (with `dst_stride` padding).
    fn combine(state: &State, dst: &mut [u8], dst_stride: usize, mode: FrameStackMode) {
        let height = state.info.height() as usize;
        let line_bytes = state.line_bytes;
        let n = state.history.len();
        debug_assert!(n >= 1);

        for y in 0..height {
            let dst_line = &mut dst[y * dst_stride..y * dst_stride + line_bytes];

            match mode {
                FrameStackMode::Lighten => {
                    // Start from the newest (which was already pushed, so back == current).
                    let newest = &state.history[n - 1];
                    let off = y * line_bytes;
                    dst_line.copy_from_slice(&newest[off..off + line_bytes]);
                    for k in 0..n - 1 {
                        let src = &state.history[k][off..off + line_bytes];
                        for (d, &s) in dst_line.iter_mut().zip(src.iter()) {
                            if s > *d {
                                *d = s;
                            }
                        }
                    }
                }
                FrameStackMode::Average => {
                    let off = y * line_bytes;
                    for x in 0..line_bytes {
                        let mut sum: u32 = 0;
                        for k in 0..n {
                            sum += state.history[k][off + x] as u32;
                        }
                        // n >= 1 (push happened earlier)
                        dst_line[x] = (sum / n as u32) as u8;
                    }
                }
                FrameStackMode::LinearDecay => {
                    // Newest frame (back, k=n-1) gets weight n; oldest (k=0) gets weight 1.
                    // Output = sum(w_k * px_k) / sum(w_k), with sum(w_k) = n*(n+1)/2.
                    let total_w: u32 = (n as u32) * (n as u32 + 1) / 2;
                    let off = y * line_bytes;
                    for x in 0..line_bytes {
                        let mut acc: u32 = 0;
                        for k in 0..n {
                            let w = (k as u32) + 1;
                            acc += w * (state.history[k][off + x] as u32);
                        }
                        dst_line[x] = (acc / total_w) as u8;
                    }
                }
            }
        }
    }

    /// Restore the alpha channel byte of every pixel from the latest frame
    /// in `history` (which is the unmodified copy of the current input).
    fn restore_alpha(state: &State, dst: &mut [u8], dst_stride: usize) {
        let alpha_offset = match state.alpha_pos {
            AlphaPos::None => return,
            AlphaPos::Last => state.pixel_stride - 1,
            AlphaPos::First => 0,
        };
        let height = state.info.height() as usize;
        let line_bytes = state.line_bytes;
        let pixel_stride = state.pixel_stride;
        let n = state.history.len();
        let newest = &state.history[n - 1];

        for y in 0..height {
            let dst_line = &mut dst[y * dst_stride..y * dst_stride + line_bytes];
            let src_line = &newest[y * line_bytes..(y + 1) * line_bytes];
            for (d_px, s_px) in dst_line
                .chunks_exact_mut(pixel_stride)
                .zip(src_line.chunks_exact(pixel_stride))
            {
                d_px[alpha_offset] = s_px[alpha_offset];
            }
        }
    }
}

impl ObjectImpl for FrameStack {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecUInt::builder("num-frames")
                    .nick("Number of frames")
                    .blurb(
                        "Number of most recent input frames combined into each output frame",
                    )
                    .minimum(MIN_NUM_FRAMES)
                    .maximum(MAX_NUM_FRAMES)
                    .default_value(DEFAULT_NUM_FRAMES)
                    .mutable_playing()
                    .build(),
                glib::ParamSpecEnum::builder_with_default("mode", DEFAULT_MODE)
                    .nick("Combination mode")
                    .blurb("How to combine the last num-frames input frames into the output")
                    .mutable_playing()
                    .build(),
                glib::ParamSpecBoolean::builder("passthrough-alpha")
                    .nick("Pass-through alpha")
                    .blurb(
                        "When the input has an alpha channel, copy the current frame's alpha \
                         instead of accumulating it",
                    )
                    .default_value(DEFAULT_PASSTHROUGH_ALPHA)
                    .mutable_playing()
                    .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "num-frames" => {
                let mut settings = self.settings.lock().unwrap();
                let num_frames: u32 = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing num-frames from {} to {}",
                    settings.num_frames,
                    num_frames
                );
                settings.num_frames = num_frames;
            }
            "mode" => {
                let mut settings = self.settings.lock().unwrap();
                let mode: FrameStackMode = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing mode from {:?} to {:?}",
                    settings.mode,
                    mode
                );
                settings.mode = mode;
            }
            "passthrough-alpha" => {
                let mut settings = self.settings.lock().unwrap();
                let passthrough_alpha: bool = value.get().expect("type checked upstream");
                gst::info!(
                    CAT,
                    imp = self,
                    "Changing passthrough-alpha from {} to {}",
                    settings.passthrough_alpha,
                    passthrough_alpha
                );
                settings.passthrough_alpha = passthrough_alpha;
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "num-frames" => settings.num_frames.to_value(),
            "mode" => settings.mode.to_value(),
            "passthrough-alpha" => settings.passthrough_alpha.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for FrameStack {}

impl ElementImpl for FrameStack {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Frame stacking accumulator",
                "Filter/Effect/Video",
                "Combines the last N input frames per pixel (lighten / average / linear decay) \
                 to produce light-trail / long-exposure / 'light HDR' effects",
                "gst-plugins-rs contributors",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst_video::VideoCapsBuilder::new()
                .format_list([
                    VideoFormat::Rgbx,
                    VideoFormat::Xrgb,
                    VideoFormat::Bgrx,
                    VideoFormat::Xbgr,
                    VideoFormat::Rgba,
                    VideoFormat::Argb,
                    VideoFormat::Bgra,
                    VideoFormat::Abgr,
                    VideoFormat::Rgb,
                    VideoFormat::Bgr,
                ])
                .build();

            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}

#[glib::object_subclass]
impl ObjectSubclass for FrameStack {
    const NAME: &'static str = "GstFrameStack";
    type Type = super::FrameStack;
    type ParentType = gst_video::VideoFilter;
}

impl BaseTransformImpl for FrameStack {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        // Drop accumulated history when the element stops, so a subsequent
        // start sees a clean slate.
        *self.state.borrow_mut() = None;
        Ok(())
    }
}

impl VideoFilterImpl for FrameStack {
    fn set_info(
        &self,
        incaps: &gst::Caps,
        in_info: &gst_video::VideoInfo,
        outcaps: &gst::Caps,
        _out_info: &gst_video::VideoInfo,
    ) -> Result<(), gst::LoggableError> {
        gst::debug!(
            CAT,
            imp = self,
            "Configured for caps {} -> {}",
            incaps,
            outcaps
        );

        let pixel_stride = in_info.format_info().pixel_stride()[0] as usize;
        let line_bytes = (in_info.width() as usize) * pixel_stride;
        let src_stride = in_info.stride()[0] as usize;

        let alpha_pos = match in_info.format() {
            VideoFormat::Rgba | VideoFormat::Bgra => AlphaPos::Last,
            VideoFormat::Argb | VideoFormat::Abgr => AlphaPos::First,
            // Rgbx/Xrgb/Bgrx/Xbgr have a padding byte we treat as opaque,
            // so we don't bother preserving it explicitly.
            _ => AlphaPos::None,
        };

        *self.state.borrow_mut() = Some(State {
            info: in_info.clone(),
            pixel_stride,
            line_bytes,
            src_stride,
            alpha_pos,
            history: VecDeque::with_capacity(MAX_NUM_FRAMES as usize),
        });

        Ok(())
    }

    fn transform_frame_ip(
        &self,
        frame: &mut gst_video::VideoFrameRef<&mut gst::BufferRef>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let settings = *self.settings.lock().unwrap();
        let mut state_guard = self.state.borrow_mut();
        let state = state_guard.as_mut().ok_or_else(|| {
            gst::element_imp_error!(self, gst::CoreError::Negotiation, ["Have no state yet"]);
            gst::FlowError::NotNegotiated
        })?;

        if state.info.format() != frame.format()
            || state.info.width() != frame.width()
            || state.info.height() != frame.height()
        {
            gst::element_imp_error!(
                self,
                gst::CoreError::Negotiation,
                ["Frame format does not match negotiated caps"]
            );
            return Err(gst::FlowError::NotNegotiated);
        }

        let dst_stride = frame.plane_stride()[0] as usize;
        let dst_data = frame.plane_data_mut(0).map_err(|_| gst::FlowError::Error)?;

        // Push the current input (= current output buffer, since AlwaysInPlace
        // means src and dst share the buffer at this point) into the ring,
        // then combine, optionally restoring the alpha channel.
        Self::push_frame(state, dst_data, settings.num_frames as usize);
        Self::combine(state, dst_data, dst_stride, settings.mode);

        if settings.passthrough_alpha {
            Self::restore_alpha(state, dst_data, dst_stride);
        }

        Ok(gst::FlowSuccess::Ok)
    }
}
