// `harletty info` on a standalone IAMF stream: what its IA sequence holds,
// from its header and descriptors and the first temporal unit that decodes.
// Reads a pipe as well as a file, like the other codecs' reports.
//
// Reported: the profiles of the sequence header, the substreams' codec, the
// mix `decode` decodes, its elements and objects, and the bed its other
// elements make. A build without the `iamf` feature reads the header alone
// and says it cannot decode the rest.

use anyhow::Result;

use super::command::InfoArgs;
use super::info_report::{IamfFacts, InfoReport};
use crate::codec_probe::iamf_profile_name;
#[cfg(not(feature = "iamf"))]
use crate::codec_probe::{NO_IAMF, find_iamf_sequence_header};
use crate::input::InputReader;

/// The report of a build that cannot read past the sequence header.
/// `reader` is the input from its first byte.
#[cfg(not(feature = "iamf"))]
pub fn cmd_info_iamf(mut reader: InputReader, args: &InfoArgs) -> Result<()> {
    let mut head = vec![0u8; 8 * 1024];
    let mut filled = 0;
    while filled < head.len() {
        match reader.read_chunk(&mut head[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    let mut report = InfoReport::not_found(NO_IAMF);
    report.codec = Some("IAMF");
    report.iamf = find_iamf_sequence_header(&head[..filled]).map(|header| IamfFacts {
        profile: iamf_profile_name(header.primary_profile),
        additional_profile: iamf_profile_name(header.additional_profile),
        codec: None,
        mix: None,
        elements: None,
        objects: None,
    });
    if args.json {
        return report.print();
    }
    if let Some(facts) = &report.iamf {
        println!("Codec        : IAMF");
        println!(
            "Profile      : {} (additional: {})",
            facts.profile, facts.additional_profile
        );
    }
    anyhow::bail!(NO_IAMF)
}

#[cfg(feature = "iamf")]
pub use with_decoder::cmd_info_iamf;

#[cfg(feature = "iamf")]
mod with_decoder {
    use super::*;
    use crate::cli::info_report::Spatial;
    use crate::iamf::{IamfReader, Sequence, Sink, Unit};
    use crate::iamf_to_oamd::SYSTEM_J_SPEAKERS;
    use truehd::structs::oamd::SpeakerLabels;

    /// Bytes read at most before a stream that has not opened a sequence
    /// is given up on.
    const MAX_BYTES: u64 = 96 * 1024 * 1024;

    /// What the head of the stream established.
    #[derive(Default)]
    struct Survey {
        sequence: Option<Sequence>,
        units: u64,
        samples: u64,
        sample_rate: u32,
    }

    impl Sink for Survey {
        fn sequence(&mut self, sequence: &Sequence) -> Result<()> {
            self.sequence.get_or_insert_with(|| sequence.clone());
            Ok(())
        }

        fn unit(&mut self, unit: Unit<'_>) -> Result<()> {
            self.units += 1;
            self.samples += unit.samples as u64;
            self.sample_rate = unit.sample_rate;
            Ok(())
        }
    }

    impl Survey {
        /// The descriptors say everything reported; a decoded unit confirms
        /// the sequence decodes and gives the rate a codec config may lack.
        fn settled(&self) -> bool {
            self.units > 0
        }

        fn seconds(&self) -> f64 {
            self.samples as f64 / f64::from(self.sample_rate.max(1))
        }
    }

    /// `reader` is the input from its first byte.
    pub fn cmd_info_iamf(mut reader: InputReader, args: &InfoArgs) -> Result<()> {
        let mut decoder = IamfReader::new(false);
        let mut survey = Survey::default();
        let max_seconds = args.max_seconds.unwrap_or(f64::INFINITY);
        let mut read = 0u64;
        let mut survey_head = || -> Result<()> {
            reader.process_chunks(64 * 1024, |chunk| {
                read += chunk.len() as u64;
                decoder.push(chunk, &mut survey)?;
                Ok(!survey.settled() && survey.seconds() < max_seconds && read < MAX_BYTES)
            })?;
            if !survey.settled() {
                // A head cut before its first whole unit still names the
                // sequence, from its descriptors.
                decoder.finish(&mut survey)?;
            }
            Ok(())
        };
        let outcome = survey_head();

        let Some(sequence) = &survey.sequence else {
            // Nothing decodes: the report says why, still naming the codec
            // when the stream opened with an IA sequence header.
            let error = match outcome {
                Err(err) => err.to_string(),
                Ok(()) => "no IA sequence header found in the input".to_owned(),
            };
            if !args.json {
                anyhow::bail!(error);
            }
            let mut report = InfoReport::not_found(&error);
            report.codec = (decoder.sequences > 0 || error.starts_with("iamf:")).then_some("IAMF");
            return report.print();
        };
        if let Err(err) = outcome {
            log::warn!("stopped reading at an error, after the sequence opened: {err}");
        }
        let sample_rate = match survey.sample_rate {
            0 => sequence.sample_rate,
            rate => rate,
        };
        let bed: Vec<SpeakerLabels> = sequence.bed.iter().map(|&j| SYSTEM_J_SPEAKERS[j]).collect();
        let heights = bed
            .iter()
            .filter(|speaker| {
                matches!(
                    speaker,
                    SpeakerLabels::Lfh
                        | SpeakerLabels::Rfh
                        | SpeakerLabels::Lrh
                        | SpeakerLabels::Rrh
                )
            })
            .count();

        if args.json {
            let mut report = InfoReport::new();
            report.codec = Some("IAMF");
            report.channels = Some(bed.len() as u32);
            report.sample_rate = (sample_rate > 0).then_some(sample_rate);
            // Objects, or a bed with heights: what a master set carries
            // beyond a plain bed. `decode` writes a master set either way.
            report.spatial = (sequence.objects > 0 || heights > 0).then(|| Spatial {
                label: damf::SourceCodec::Iamf.label().to_string(),
                kind: "iamf",
                objects: Some(sequence.objects as u32),
                fixed: None,
                experimental: false,
                presentation: None,
            });
            report.iamf = Some(IamfFacts {
                profile: iamf_profile_name(sequence.primary_profile),
                additional_profile: iamf_profile_name(sequence.additional_profile),
                codec: Some(sequence.codec),
                mix: Some(sequence.mix_id),
                elements: Some(sequence.elements as u32),
                objects: Some(sequence.objects as u32),
            });
            report.frames_seen = survey.units;
            report.seconds_seen = survey.seconds();
            return report.print();
        }

        let names: Vec<String> = bed.iter().map(|speaker| format!("{speaker:?}")).collect();
        println!("Codec        : IAMF ({})", sequence.codec);
        println!(
            "Profile      : {} (additional: {})",
            iamf_profile_name(sequence.primary_profile),
            iamf_profile_name(sequence.additional_profile)
        );
        println!("Mix          : {}", sequence.mix_id);
        println!("Elements     : {}", sequence.elements);
        println!("Objects      : {}", sequence.objects);
        println!(
            "Bed          : {}",
            if names.is_empty() {
                "none".to_owned()
            } else {
                names.join(" ")
            }
        );
        if sample_rate > 0 {
            println!("Sample rate  : {sample_rate} Hz");
        }
        println!("Units seen   : {}", survey.units);
        Ok(())
    }
}
