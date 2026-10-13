// SPDX-License-Identifier: Apache-2.0
//
// Probe the DTS:X extension of a lossy DTS-HD stream (core + XXCH, no XLL):
// decode every frame through the HD decoder, report what came out of the
// bed and the extension, and compare the bed with an ffmpeg f32 reference
// (interleaved 7.1 in ffmpeg order) when one is given.
// Usage: cargo run -p dca --example lossy_x_probe -- <file.dts> [ref.f32]

use std::collections::BTreeMap;
use std::io::Read;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: lossy_x_probe <file.dts> [ref.f32]");
    let reference: Option<Vec<f32>> = args.next().map(|p| {
        std::fs::read(p)
            .expect("read reference")
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    });
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .expect("open")
        .read_to_end(&mut bytes)
        .expect("read");

    // ffmpeg 7.1 order -> DCA speaker index.
    const FFMPEG_ORDER: [usize; 8] = [1, 2, 0, 5, 7, 8, 3, 4];
    let mut dec = dca::HdDecoder::new();
    let mut core_dec = dca::PcmDecoder::new();
    let mut lfe_cmp = [0f64; 4]; // sq(float*k - ref), sq(fixed - ref), sq(fixed - float*k), sq(ref)
    const K: f64 = 92682.0 / 65536.0;
    let mut pos = 0usize;
    let mut frame = 0usize;
    let mut kinds: BTreeMap<String, usize> = Default::default();
    let mut x_errors: BTreeMap<String, usize> = Default::default();
    let mut xxch_errors: BTreeMap<String, usize> = Default::default();
    let mut meta_ok = 0usize;
    let mut feeds_ok = 0usize;
    let mut masks: BTreeMap<u32, usize> = Default::default();
    let mut sq_err = [0f64; 8];
    let mut sq_ref = [0f64; 8];
    let mut sq_ours = [0f64; 8];
    let mut x_energy = [0f64; 4];
    let mut x_dot_bed = [0f64; 4]; // feed vs the bed channel it folds into (L, R, Lsr, Rsr)
    let mut bed_energy = [0f64; 4];
    let mut sample_pos = 0usize;
    let mut dump: Vec<f32> = Vec::new(); // 12 channels interleaved: ffmpeg 7.1 order + 4 feeds
    while pos + 16 <= bytes.len() {
        let info = match dca::parse_header(&bytes[pos..]) {
            Ok(i) => i,
            Err(_) => break,
        };
        let fs = info.frame_size;
        let exss_start = pos + fs;
        if exss_start + 4 > bytes.len()
            || bytes[exss_start..exss_start + 4] != dca::SYNCWORD_SUBSTREAM.to_be_bytes()
        {
            break;
        }
        let es = dca::exss_substream_size(&bytes[exss_start..]).expect("exss size");
        let exss = &bytes[exss_start..exss_start + es];
        *kinds
            .entry(format!("{:?}", dca::exss_kind(exss)))
            .or_default() += 1;
        match dec.decode(&bytes[pos..exss_start], exss) {
            Ok(hd) => {
                *masks.entry(hd.output_mask).or_default() += 1;
                if let Some(e) = hd.xxch_decode_error {
                    *xxch_errors.entry(e.to_string()).or_default() += 1;
                }
                if let Some(e) = hd.x_decode_error {
                    *x_errors.entry(e.to_string()).or_default() += 1;
                }
                let n = hd.bed_sample_count();
                if hd.x_samples.len() == 4 && hd.x_samples.iter().all(|c| c.len() == n) {
                    feeds_ok += 1;
                    for (i, feed) in hd.x_samples.iter().enumerate() {
                        let spkr = [1usize, 2, 7, 8][i];
                        let bed = hd.samples[spkr].as_ref().unwrap();
                        for (s, (&x, &b)) in feed.iter().zip(bed.iter()).enumerate() {
                            let _ = s;
                            x_energy[i] += (x as f64) * (x as f64);
                            x_dot_bed[i] += (x as f64) * (b as f64);
                            bed_energy[i] += (b as f64) * (b as f64);
                        }
                    }
                }
                if dca::XMetadata::parse(&hd.x_payload, 4).is_ok() {
                    meta_ok += 1;
                }
                if let (Some(reference), Ok(push)) = (
                    &reference,
                    core_dec.push_access_unit(&bytes[pos..exss_start]),
                ) {
                    if let (Some(float_lfe), Some(fixed_lfe)) =
                        (push.pcm.lfe_channel.as_ref(), hd.samples[5].as_ref())
                    {
                        for (s, (&fl, &fx)) in float_lfe.iter().zip(fixed_lfe.iter()).enumerate() {
                            let Some(&r) = reference.get((sample_pos + s) * 8 + 3) else {
                                break;
                            };
                            let (fl, fx, r) = (fl as f64 * K, fx as f64, r as f64);
                            lfe_cmp[0] += (fl - r).powi(2);
                            lfe_cmp[1] += (fx - r).powi(2);
                            lfe_cmp[2] += (fx - fl).powi(2);
                            lfe_cmp[3] += r.powi(2);
                        }
                    }
                }
                if let Some(reference) = &reference {
                    for (k, &spkr) in FFMPEG_ORDER.iter().enumerate() {
                        let Some(ch) = hd.samples[spkr].as_ref() else {
                            continue;
                        };
                        for (s, &v) in ch.iter().enumerate() {
                            let Some(&r) = reference.get((sample_pos + s) * 8 + k) else {
                                break;
                            };
                            sq_err[k] += ((v - r) as f64).powi(2);
                            sq_ref[k] += (r as f64).powi(2);
                            sq_ours[k] += (v as f64).powi(2);
                        }
                    }
                }
                for s in 0..n {
                    for &spkr in &FFMPEG_ORDER {
                        dump.push(hd.samples[spkr].as_ref().map_or(0.0, |c| c[s]));
                    }
                    for i in 0..4 {
                        dump.push(hd.x_samples.get(i).map_or(0.0, |c| c[s]));
                    }
                }
                sample_pos += n;
                if frame < 2 {
                    println!(
                        "frame {frame}: mask={:#x} bed_samples={n} x_present={} feeds={} x_bits={} x_err={:?} xxch_err={:?}",
                        hd.output_mask,
                        hd.x_present,
                        hd.x_samples.len(),
                        hd.x_bits_consumed,
                        hd.x_decode_error,
                        hd.xxch_decode_error
                    );
                }
            }
            Err(e) => {
                *x_errors.entry(format!("decode error {e:?}")).or_default() += 1;
            }
        }
        pos = exss_start + es;
        frame += 1;
    }
    if let Ok(out) = std::env::var("LOSSY_X_DUMP") {
        let bytes: Vec<u8> = dump.iter().flat_map(|v| v.to_le_bytes()).collect();
        std::fs::write(&out, bytes).expect("write dump");
        println!("dumped {} samples x 12 channels to {out}", dump.len() / 12);
    }
    println!("frames={frame} kinds={kinds:?} masks={masks:?}");
    println!(
        "metadata_ok={meta_ok} feeds_ok={feeds_ok} x_errors={x_errors:?} xxch_errors={xxch_errors:?}"
    );
    for i in 0..4 {
        let rms = (x_energy[i] / sample_pos.max(1) as f64).sqrt();
        let corr = x_dot_bed[i] / (x_energy[i] * bed_energy[i]).sqrt().max(1e-30);
        println!(
            "  feed {i}: rms={rms:.5} ({:.1} dBFS) corr_with_fold_target={corr:.3}",
            20.0 * rms.max(1e-9).log10()
        );
    }
    if reference.is_some() {
        let n = sample_pos.max(1) as f64;
        println!(
            "  LFE check: rmse(float core x1.414, ref)={:.3e} rmse(fixed hd, ref)={:.3e} rmse(fixed, float x1.414)={:.3e} ref_rms={:.3e}",
            (lfe_cmp[0] / n).sqrt(),
            (lfe_cmp[1] / n).sqrt(),
            (lfe_cmp[2] / n).sqrt(),
            (lfe_cmp[3] / n).sqrt()
        );
        let names = ["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"];
        for k in 0..8 {
            let rmse = (sq_err[k] / sample_pos.max(1) as f64).sqrt();
            let rms = (sq_ref[k] / sample_pos.max(1) as f64).sqrt();
            let ours = (sq_ours[k] / sample_pos.max(1) as f64).sqrt();
            println!(
                "  bed {}: rmse_vs_ffmpeg={rmse:.3e} ref_rms={rms:.5} our_rms={ours:.5} ratio={:.4}",
                names[k],
                ours / rms.max(1e-12)
            );
        }
    }
}
