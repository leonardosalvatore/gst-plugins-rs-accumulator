// Copyright (C) 2026 gst-plugins-rs contributors
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0
/**
 * element-framestack:
 * @short_description: Temporally accumulates the last N input frames using
 * a configurable per-pixel combination mode.
 *
 * The framestack element keeps a ring buffer of the most recent input frames
 * and combines them per pixel/channel into the output frame, producing
 * "light HDR" / long-exposure / light-trail style effects without changing
 * the framerate (one output frame per input frame, with the same PTS).
 *
 * The number of frames considered is controlled by the #framestack:num-frames
 * property, the combination mode by #framestack:mode (see #GstFrameStackMode)
 * and alpha handling by #framestack:passthrough-alpha for RGBA/BGRA-style
 * formats.
 *
 * ## Example pipeline
 * ```bash
 * gst-launch-1.0 videotestsrc pattern=ball ! framestack num-frames=12 mode=lighten ! \
 *   videoconvert ! autovideosink
 * ```
 *
 * Since: plugins-rs-0.16.0
 */
use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct FrameStack(ObjectSubclass<imp::FrameStack>) @extends gst_video::VideoFilter, gst_base::BaseTransform, gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "framestack",
        gst::Rank::NONE,
        FrameStack::static_type(),
    )
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy, Default, glib::Enum)]
#[repr(u32)]
#[enum_type(name = "GstFrameStackMode")]
#[non_exhaustive]
pub enum FrameStackMode {
    /// Per-channel max over the last N frames (light trails / "light HDR").
    #[default]
    #[enum_value(
        name = "Lighten: per-channel max over the last N frames (light trails).",
        nick = "lighten"
    )]
    Lighten = 0,

    /// Per-channel mean over the last N frames (long-exposure look).
    #[enum_value(
        name = "Average: per-channel mean over the last N frames (long-exposure).",
        nick = "average"
    )]
    Average = 1,

    /// Weighted sum, newest frame full weight, oldest weight 1, normalized.
    #[enum_value(
        name = "LinearDecay: weighted blend with newest at full weight, decaying linearly to the oldest.",
        nick = "linear-decay"
    )]
    LinearDecay = 2,
}
