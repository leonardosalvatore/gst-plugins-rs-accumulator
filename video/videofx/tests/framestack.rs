// Copyright (C) 2026 gst-plugins-rs contributors
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::prelude::*;
use gstrsvideofx::FrameStackMode;

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(|| {
        gst::init().unwrap();
        gstrsvideofx::plugin_register_static().expect("Failed to register rsvideofx plugin");
    });
}

/// Build a 4x2 RGB buffer where every pixel has the given (r, g, b).
fn solid_rgb_buffer_4x2(r: u8, g: u8, b: u8) -> gst::Buffer {
    // 4 px wide, RGB (3 bytes per pixel), 2 rows. Stride is 12 (already aligned).
    let mut data = Vec::with_capacity(4 * 3 * 2);
    for _ in 0..(4 * 2) {
        data.extend_from_slice(&[r, g, b]);
    }
    gst::Buffer::from_mut_slice(data)
}

/// Build a 4x2 RGBA buffer where every pixel has the given (r, g, b, a).
fn solid_rgba_buffer_4x2(r: u8, g: u8, b: u8, a: u8) -> gst::Buffer {
    let mut data = Vec::with_capacity(4 * 4 * 2);
    for _ in 0..(4 * 2) {
        data.extend_from_slice(&[r, g, b, a]);
    }
    gst::Buffer::from_mut_slice(data)
}

fn make_harness(mode: FrameStackMode, num_frames: u32, caps: &str) -> gst_check::Harness {
    let mut h = gst_check::Harness::new("framestack");
    h.set_src_caps_str(caps);
    h.set_sink_caps_str(caps);
    let element = h.element().expect("framestack element");
    element.set_property("mode", mode);
    element.set_property("num-frames", num_frames);
    h
}

fn pull_rgb_payload(h: &mut gst_check::Harness) -> Vec<u8> {
    let buf = h.pull().expect("Couldn't pull buffer");
    let map = buf.map_readable().expect("buffer should be readable");
    map.to_vec()
}

#[test]
fn test_lighten_keeps_max_per_channel() {
    init();
    let caps = "video/x-raw,format=RGB,width=4,height=2,framerate=25/1";
    let mut h = make_harness(FrameStackMode::Lighten, 4, caps);

    // First frame: dim red.
    h.push(solid_rgb_buffer_4x2(64, 0, 0)).unwrap();
    let out1 = pull_rgb_payload(&mut h);
    assert!(
        out1.chunks_exact(3).all(|p| p == [64, 0, 0]),
        "first lighten frame should equal input"
    );

    // Second frame: bright green; lighten => per-channel max = (64, 255, 0).
    h.push(solid_rgb_buffer_4x2(0, 255, 0)).unwrap();
    let out2 = pull_rgb_payload(&mut h);
    assert!(
        out2.chunks_exact(3).all(|p| p == [64, 255, 0]),
        "lighten of (64,0,0) and (0,255,0) should be (64,255,0), got {:?}",
        &out2[..3]
    );

    // Third frame: pure white. Lighten => (255, 255, 255).
    h.push(solid_rgb_buffer_4x2(255, 255, 255)).unwrap();
    let out3 = pull_rgb_payload(&mut h);
    assert!(
        out3.chunks_exact(3).all(|p| p == [255, 255, 255]),
        "lighten with white should saturate to white"
    );
}

#[test]
fn test_average_two_frames_is_midpoint() {
    init();
    let caps = "video/x-raw,format=RGB,width=4,height=2,framerate=25/1";
    let mut h = make_harness(FrameStackMode::Average, 2, caps);

    // First frame: black. With one frame in history, average == black.
    h.push(solid_rgb_buffer_4x2(0, 0, 0)).unwrap();
    let out1 = pull_rgb_payload(&mut h);
    assert!(out1.iter().all(|&b| b == 0));

    // Second frame: white. With num-frames=2, average of (0,0,0) and
    // (255,255,255) is (127,127,127) using integer division (255/2 == 127).
    h.push(solid_rgb_buffer_4x2(255, 255, 255)).unwrap();
    let out2 = pull_rgb_payload(&mut h);
    assert!(
        out2.chunks_exact(3).all(|p| p == [127, 127, 127]),
        "average of black+white should be mid-gray (127), got {:?}",
        &out2[..3]
    );

    // Third frame: black. The history now contains (white, black), so the
    // average should drop back to (127, 127, 127) too.
    h.push(solid_rgb_buffer_4x2(0, 0, 0)).unwrap();
    let out3 = pull_rgb_payload(&mut h);
    assert!(
        out3.chunks_exact(3).all(|p| p == [127, 127, 127]),
        "average should slide with the ring, got {:?}",
        &out3[..3]
    );
}

#[test]
fn test_linear_decay_weights_newest_more() {
    init();
    let caps = "video/x-raw,format=RGB,width=4,height=2,framerate=25/1";
    let mut h = make_harness(FrameStackMode::LinearDecay, 2, caps);

    // First frame: black.
    h.push(solid_rgb_buffer_4x2(0, 0, 0)).unwrap();
    let _ = pull_rgb_payload(&mut h);

    // Second frame: white. With num-frames=2 the weights are 1 (oldest=black)
    // and 2 (newest=white), normalized by 3, so the output is
    // (1*0 + 2*255) / 3 = 170.
    h.push(solid_rgb_buffer_4x2(255, 255, 255)).unwrap();
    let out2 = pull_rgb_payload(&mut h);
    assert!(
        out2.chunks_exact(3).all(|p| p == [170, 170, 170]),
        "linear decay should weight newest (white) more, got {:?}",
        &out2[..3]
    );
}

#[test]
fn test_passthrough_alpha_preserves_current_alpha() {
    init();
    let caps = "video/x-raw,format=RGBA,width=4,height=2,framerate=25/1";
    let mut h = gst_check::Harness::new("framestack");
    h.set_src_caps_str(caps);
    h.set_sink_caps_str(caps);
    let element = h.element().unwrap();
    element.set_property("mode", FrameStackMode::Lighten);
    element.set_property("num-frames", 2u32);
    element.set_property("passthrough-alpha", true);

    // Frame 1: opaque black.
    h.push(solid_rgba_buffer_4x2(0, 0, 0, 255)).unwrap();
    let _ = pull_rgb_payload(&mut h);

    // Frame 2: half-transparent white.
    h.push(solid_rgba_buffer_4x2(255, 255, 255, 128)).unwrap();
    let out2 = pull_rgb_payload(&mut h);

    // Lighten on RGB => (255,255,255). Alpha should NOT be the lighten
    // result (255) but the current input's alpha (128).
    assert!(
        out2.chunks_exact(4).all(|p| p == [255, 255, 255, 128]),
        "alpha should be passed through from current frame, got {:?}",
        &out2[..4]
    );
}

#[test]
fn test_history_resets_on_caps_change() {
    init();

    // Start with one caps configuration.
    let caps_a = "video/x-raw,format=RGB,width=4,height=2,framerate=25/1";
    let mut h = make_harness(FrameStackMode::Lighten, 4, caps_a);

    h.push(solid_rgb_buffer_4x2(0, 0, 255)).unwrap();
    let _ = pull_rgb_payload(&mut h);
    h.push(solid_rgb_buffer_4x2(255, 0, 0)).unwrap();
    let _ = pull_rgb_payload(&mut h);

    // Switch to a different resolution; this must reset the history,
    // otherwise the leftover blue from the previous caps would taint the
    // new dim-red output.
    let caps_b = "video/x-raw,format=RGB,width=8,height=4,framerate=25/1";
    h.set_src_caps_str(caps_b);
    h.set_sink_caps_str(caps_b);

    // 8x4 dim red. With a clean history this should round-trip unchanged.
    let mut data = Vec::with_capacity(8 * 4 * 3);
    for _ in 0..(8 * 4) {
        data.extend_from_slice(&[10, 0, 0]);
    }
    let buf = gst::Buffer::from_mut_slice(data);
    h.push(buf).unwrap();
    let out = pull_rgb_payload(&mut h);
    assert!(
        out.chunks_exact(3).all(|p| p == [10, 0, 0]),
        "after a caps change the previous-format frames should be discarded"
    );
}
