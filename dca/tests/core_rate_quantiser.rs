// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Regression: the core decoder picked the lossless quantiser table when the
// header's 5-bit rate *code* was 3. Code 3 is 96 kb/s; the lossless sentinel
// is the rate value 3 that code 31 names (ffmpeg dca_core.c compares
// `ff_dca_bit_rates[br_code]`). Every 96 kb/s core decoded to noise.
//
// The fixture is 10 core-only frames (mono, 16 kHz, 96 kb/s, 384 bytes each)
// of a synthetic 1 kHz tone at half scale, made with libavcodec's encoder:
//
//   ffmpeg -f lavfi -i "aevalsrc=0.5*sin(2*PI*1000*t):s=16000:d=0.32" \
//       -f s32le -ac 1 tone.s32
//   ffmpeg -f s32le -ar 16000 -ch_layout mono -i tone.s32 \
//       -c:a dca -strict -2 -b:a 96000 -f dts dts_core_96k_mono_tone_10f.dts
//
// libavcodec decodes it to the tone at 104 dB SNR from the third frame on.
// To compare against that decode directly, set HARLETTY_DTS_96K_REFERENCE
// to the output of `ffmpeg -i dts_core_96k_mono_tone_10f.dts -f f32le ref.f32`.

use dca::{CorePcmFrame, Extractor, PcmDecoder};

const FIXTURE: &[u8] =
    include_bytes!("../../harletty/tests/fixtures/dts_core_96k_mono_tone_10f.dts");
const SAMPLE_RATE: f64 = 16_000.0;
const TONE_HZ: f64 = 1_000.0;
const AMPLITUDE: f64 = 0.5;
const SAMPLES_PER_FRAME: usize = 512;
/// The first two frames hold the encoder's start-up transient.
const SETTLED_FROM: usize = 2 * SAMPLES_PER_FRAME;

fn decode() -> Vec<f32> {
    let mut extractor = Extractor::default();
    extractor.push_bytes(FIXTURE);
    let mut decoder = PcmDecoder::new();
    let mut pcm = CorePcmFrame::default();
    let mut out = Vec::new();
    while let Some(frame) = extractor.next_frame().expect("extract") {
        decoder
            .decode_into(frame.as_bytes(), &mut pcm)
            .expect("decode");
        assert_eq!(pcm.fullband_channels.len(), 1, "mono core");
        out.extend_from_slice(&pcm.fullband_channels[0]);
    }
    out
}

/// The 5-bit rate code of the first frame's core header.
fn rate_code(data: &[u8]) -> u64 {
    // After the sync word: normal(1) deficit(5) crc(1) npcm(7) fsize(14)
    // amode(6) sr(4), then the rate code at bits 38..43.
    let bits = u64::from_be_bytes(data[4..12].try_into().unwrap());
    (bits >> (64 - 43)) & 0x1f
}

/// SNR in dB of `x` against the tone, fitted in phase over whole periods.
fn tone_snr(x: &[f32], first_sample: usize) -> (f64, f64) {
    let w = 2.0 * std::f64::consts::PI * TONE_HZ / SAMPLE_RATE;
    let n = x.len() as f64;
    let (mut a, mut b) = (0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let t = w * (first_sample + i) as f64;
        a += f64::from(v) * t.sin();
        b += f64::from(v) * t.cos();
    }
    let (a, b) = (2.0 * a / n, 2.0 * b / n);
    let (mut signal, mut noise) = (0.0, 0.0);
    for (i, &v) in x.iter().enumerate() {
        let t = w * (first_sample + i) as f64;
        let fit = a * t.sin() + b * t.cos();
        signal += fit * fit;
        noise += (f64::from(v) - fit).powi(2);
    }
    (
        a.hypot(b),
        10.0 * (signal / noise.max(f64::MIN_POSITIVE)).log10(),
    )
}

#[test]
fn rate_code_3_is_96_kbps_not_lossless() {
    assert_eq!(rate_code(FIXTURE), 3, "fixture must carry rate code 3");

    let out = decode();
    assert_eq!(out.len(), 10 * SAMPLES_PER_FRAME);

    // 8 settled frames = 4096 samples, a whole number of 16-sample periods.
    let (amplitude, snr) = tone_snr(&out[SETTLED_FROM..], SETTLED_FROM);
    assert!(
        (amplitude - AMPLITUDE).abs() < 1e-3,
        "tone amplitude {amplitude:.5}, expected {AMPLITUDE}"
    );
    // The lossless table gave 0.4 dB here; libavcodec decodes 104 dB.
    assert!(snr > 90.0, "tone SNR {snr:.2} dB");
}

#[test]
fn rate_code_3_matches_ffmpeg_reference() {
    let Ok(reference) = std::env::var("HARLETTY_DTS_96K_REFERENCE") else {
        eprintln!("skipping: HARLETTY_DTS_96K_REFERENCE is not set");
        return;
    };
    let reference: Vec<f32> = std::fs::read(reference)
        .expect("read reference")
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let out = decode();
    assert_eq!(out.len(), reference.len());
    let (mut signal, mut noise) = (0.0f64, 0.0f64);
    for (&r, &o) in reference.iter().zip(&out) {
        signal += f64::from(r).powi(2);
        noise += (f64::from(o) - f64::from(r)).powi(2);
    }
    let snr = 10.0 * (signal / noise.max(f64::MIN_POSITIVE)).log10();
    eprintln!("SNR against libavcodec: {snr:.2} dB");
    assert!(snr > 60.0, "SNR against libavcodec {snr:.2} dB");
}
