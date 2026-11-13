//! Filling the samples a stream is missing, and smoothing a join where it
//! repeated time, from the samples played around them.
//!
//! Where a remux dropped an access unit at a seamless branch, the stream's
//! timing says how many samples are missing, and both sides of the hole are
//! known when it is found: the samples just played, and the access unit that
//! follows it. [`Concealer::fill`] interpolates the hole as audio restoration
//! does for short gaps: an autoregressive model, fitted by Burg's method on the
//! samples before the hole, predicts forwards from its left side and backwards
//! from its right side, and the two predictions are cross-faded across it. A
//! gap of an access unit is under a millisecond, shorter than a period of most
//! of what a film carries, and this lands far closer to the missing samples
//! than silence, which drops every channel to zero and back.
//!
//! Where a stream played two renderings of the same time, the one kept and the
//! one dropped seldom match bit for bit, and what comes next follows on from
//! the dropped one. [`Concealer::note_overlap`] measures the difference where
//! they end and [`Concealer::smooth`] fades it out over the samples after the
//! join, instead of leaving a step there.
//!
//! Rows are the decoders' sample-major layout, `[i32; 16]`, 24-bit samples with
//! the first `channels` columns in use. Keeping the history costs a copy of the
//! rows played; [`Concealer::fill`] allocates its working vectors, since it
//! runs where a stream has a hole, not for every access unit.

/// Columns of a row: the most channels a decoder writes.
pub const MAX_CHANNELS: usize = 16;

/// One sample of every channel.
pub type Row = [i32; MAX_CHANNELS];

/// Rows of history kept: 21 ms at 48 kHz, the model's whole fitting window.
pub const HISTORY: usize = 1024;

/// Order of the autoregressive model.
pub const ORDER: usize = 32;

const MAX_24: f64 = ((1 << 23) - 1) as f64;
const MIN_24: f64 = -(1 << 23) as f64;

/// The last rows a stream played, to fill a hole and smooth a join from.
pub struct Concealer {
    ring: Box<[Row]>,
    /// Where the next row goes.
    next: usize,
    /// Rows held, at most [`HISTORY`].
    len: usize,
    /// Channels of the rows held.
    channels: usize,
    /// Per channel, what to fade out over the next rows played.
    pending: Option<[f64; MAX_CHANNELS]>,
}

impl Default for Concealer {
    fn default() -> Self {
        Self {
            ring: vec![[0; MAX_CHANNELS]; HISTORY].into_boxed_slice(),
            next: 0,
            len: 0,
            channels: 0,
            pending: None,
        }
    }
}

impl Concealer {
    /// Forgets what was played, as at the start of a stream.
    pub fn reset(&mut self) {
        self.next = 0;
        self.len = 0;
        self.channels = 0;
        self.pending = None;
    }

    /// Records rows played. Rows of another channel count start the history
    /// over: a new layout does not continue the old one.
    pub fn push(&mut self, rows: &[Row], channels: usize) {
        if channels != self.channels {
            self.reset();
            self.channels = channels;
        }
        for row in rows {
            self.ring[self.next] = *row;
            self.next = (self.next + 1) % HISTORY;
        }
        self.len = (self.len + rows.len()).min(HISTORY);
    }

    /// The rows held, oldest first, of one channel.
    fn channel(&self, channel: usize) -> Vec<f64> {
        let start = (self.next + HISTORY - self.len) % HISTORY;
        (0..self.len)
            .map(|i| f64::from(self.ring[(start + i) % HISTORY][channel]))
            .collect()
    }

    fn last(&self) -> Option<&Row> {
        (self.len > 0).then(|| &self.ring[(self.next + HISTORY - 1) % HISTORY])
    }

    /// Fills `gap` with what most likely sat between the rows played and
    /// `next`, the rows that follow the hole, in `channels` channels.
    ///
    /// With too little history in that layout to fit a model on, the hole is
    /// a straight line from the last sample played (or zero) to the first of
    /// `next` (or zero). The fill joins both sides by itself: an overlap noted
    /// before it has nothing left to smooth.
    pub fn fill(&mut self, gap: &mut [Row], next: &[Row], channels: usize) {
        self.pending = None;
        for row in gap.iter_mut() {
            *row = [0; MAX_CHANNELS];
        }
        if gap.is_empty() {
            return;
        }
        let channels = channels.min(MAX_CHANNELS);
        let modelled = channels == self.channels && self.len >= 2 * ORDER;
        let mut model = vec![0.0; ORDER + 1];
        for channel in 0..channels {
            let right: Vec<f64> = next.iter().map(|r| f64::from(r[channel])).collect();
            let filled = if modelled {
                let left = self.channel(channel);
                burg(&left, ORDER, &mut model);
                interpolate(&left, &right, &model, gap.len())
            } else {
                let from = if channels == self.channels {
                    self.last().map_or(0.0, |r| f64::from(r[channel]))
                } else {
                    0.0
                };
                let to = right.first().copied().unwrap_or(0.0);
                let steps = (gap.len() + 1) as f64;
                (1..=gap.len())
                    .map(|i| from + (to - from) * i as f64 / steps)
                    .collect()
            };
            for (row, value) in gap.iter_mut().zip(filled) {
                row[channel] = value.round().clamp(MIN_24, MAX_24) as i32;
            }
        }
    }

    /// Notes that `dropped`, a second rendering of the time the last rows
    /// played cover, was not played: what comes next follows on from it, not
    /// from what was played, and [`smooth`](Self::smooth) evens that out.
    pub fn note_overlap(&mut self, dropped: &[Row], channels: usize) {
        let (Some(played), Some(dropped)) = (self.last(), dropped.last()) else {
            return;
        };
        if channels != self.channels {
            return;
        }
        let mut step = [0.0; MAX_CHANNELS];
        for (channel, step) in step.iter_mut().enumerate().take(channels) {
            *step = f64::from(played[channel]) - f64::from(dropped[channel]);
        }
        self.pending = step.iter().any(|&s| s != 0.0).then_some(step);
    }

    /// Whether [`smooth`](Self::smooth) has a join to even out.
    pub fn has_overlap(&self) -> bool {
        self.pending.is_some()
    }

    /// Evens out the join an overlap left before `rows`, the rows played next:
    /// they start where the kept rendering ended, and the difference fades out
    /// across them.
    pub fn smooth(&mut self, rows: &mut [Row], channels: usize) {
        let Some(step) = self.pending.take() else {
            return;
        };
        if channels != self.channels {
            return;
        }
        let span = (rows.len() + 1) as f64;
        for (i, row) in rows.iter_mut().enumerate() {
            let weight = 0.5 * (1.0 + (std::f64::consts::PI * (i + 1) as f64 / span).cos());
            for channel in 0..channels.min(MAX_CHANNELS) {
                let value = f64::from(row[channel]) + step[channel] * weight;
                row[channel] = value.round().clamp(MIN_24, MAX_24) as i32;
            }
        }
    }
}

/// Burg's method: the coefficients `a` (with `a[0] = 1`) of the order-`order`
/// model `x[t] ≈ -Σ a[i]·x[t-i]` fitted on `x`. The filter it gives is minimum
/// phase, so a prediction run from it decays rather than grows.
fn burg(x: &[f64], order: usize, a: &mut [f64]) {
    a.fill(0.0);
    a[0] = 1.0;
    let n = x.len() - 1;
    let mut f = x.to_vec();
    let mut b = x.to_vec();
    let mut dk: f64 = 2.0 * f.iter().map(|v| v * v).sum::<f64>() - f[0] * f[0] - b[n] * b[n];
    for k in 0..order {
        if dk <= f64::MIN_POSITIVE {
            break;
        }
        let mut mu = 0.0;
        for i in 0..n - k {
            mu += f[i + k + 1] * b[i];
        }
        mu *= -2.0 / dk;
        for i in 0..=k.div_ceil(2) {
            let lower = a[i] + mu * a[k + 1 - i];
            let upper = a[k + 1 - i] + mu * a[i];
            a[i] = lower;
            a[k + 1 - i] = upper;
        }
        for i in 0..n - k {
            let forward = f[i + k + 1] + mu * b[i];
            let backward = b[i] + mu * f[i + k + 1];
            f[i + k + 1] = forward;
            b[i] = backward;
        }
        dk = (1.0 - mu * mu) * dk - f[k + 1] * f[k + 1] - b[n - k - 1] * b[n - k - 1];
    }
}

/// `length` samples between `left` and `right`: the model run forwards from
/// the end of `left` and backwards from the start of `right` (a stationary
/// process predicts backwards with the same coefficients), cross-faded.
fn interpolate(left: &[f64], right: &[f64], a: &[f64], length: usize) -> Vec<f64> {
    let order = a.len() - 1;

    let mut forward = left[left.len() - order..].to_vec();
    for _ in 0..length {
        let t = forward.len();
        let prediction = -(1..=order).map(|i| a[i] * forward[t - i]).sum::<f64>();
        forward.push(prediction);
    }
    let forward = &forward[order..];

    let reach = order.min(right.len());
    let mut backward: Vec<f64> = right[..reach].iter().rev().copied().collect();
    for _ in 0..length {
        let t = backward.len();
        let prediction = -(1..=reach).map(|i| a[i] * backward[t - i]).sum::<f64>();
        backward.push(prediction);
    }
    let backward: Vec<f64> = backward[reach..].iter().rev().copied().collect();

    let span = (length + 1) as f64;
    (0..length)
        .map(|i| {
            let weight = if reach == 0 {
                0.0
            } else {
                0.5 * (1.0 - (std::f64::consts::PI * (i + 1) as f64 / span).cos())
            };
            forward[i] * (1.0 - weight) + backward[i] * weight
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two tones and a little noise, 24-bit, on two channels (the second one
    /// inverted): `count` rows from sample `start`.
    fn music(start: usize, count: usize) -> Vec<Row> {
        let mut noise = 0x2545_f491_u32.wrapping_add((start as u32).wrapping_mul(2_654_435_761));
        (start..start + count)
            .map(|t| {
                noise ^= noise << 13;
                noise ^= noise >> 17;
                noise ^= noise << 5;
                let t = t as f64 / 48_000.0;
                let tones = 0.4 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()
                    + 0.2 * (2.0 * std::f64::consts::PI * 1250.0 * t + 0.3).sin();
                let value = tones + 1e-4 * (f64::from(noise) / f64::from(u32::MAX) - 0.5);
                let sample = (value * MAX_24) as i32;
                let mut row = [0; MAX_CHANNELS];
                row[0] = sample;
                row[1] = -sample;
                row
            })
            .collect()
    }

    fn rms(rows: &[Row], channel: usize) -> f64 {
        (rows
            .iter()
            .map(|r| f64::from(r[channel]).powi(2))
            .sum::<f64>()
            / rows.len() as f64)
            .sqrt()
    }

    fn error(a: &[Row], b: &[Row], channel: usize) -> f64 {
        (a.iter()
            .zip(b)
            .map(|(a, b)| f64::from(a[channel] - b[channel]).powi(2))
            .sum::<f64>()
            / a.len() as f64)
            .sqrt()
    }

    #[test]
    fn a_hole_in_music_is_filled_close_to_what_was_there() {
        let played = music(0, 2000);
        let missing = music(2000, 40);
        let next = music(2040, 40);
        let mut concealer = Concealer::default();
        concealer.push(&played, 2);

        let mut gap = vec![[0; MAX_CHANNELS]; 40];
        concealer.fill(&mut gap, &next, 2);

        for channel in 0..2 {
            let relative = error(&gap, &missing, channel) / rms(&missing, channel);
            eprintln!("40-sample hole, channel {channel}: error {relative:.2e} of the signal RMS");
            // Silence would be 1.0 here.
            assert!(relative < 0.01, "channel {channel}: {relative}");
        }
        assert!(gap.iter().all(|r| r[2..].iter().all(|&v| v == 0)));
    }

    #[test]
    fn a_longer_hole_is_still_far_closer_than_silence() {
        let played = music(0, 2000);
        let missing = music(2000, 400);
        let next = music(2400, 40);
        let mut concealer = Concealer::default();
        concealer.push(&played, 2);

        let mut gap = vec![[0; MAX_CHANNELS]; 400];
        concealer.fill(&mut gap, &next, 2);

        let relative = error(&gap, &missing, 0) / rms(&missing, 0);
        eprintln!("400-sample hole: error {relative:.2e} of the signal RMS");
        assert!(relative < 0.1, "{relative}");
    }

    #[test]
    fn silence_stays_silent() {
        let mut concealer = Concealer::default();
        concealer.push(&vec![[0; MAX_CHANNELS]; 500], 6);
        let mut gap = vec![[7; MAX_CHANNELS]; 40];
        concealer.fill(&mut gap, &[[0; MAX_CHANNELS]; 40], 6);
        assert!(gap.iter().all(|r| r.iter().all(|&v| v == 0)));
    }

    /// Without history in the layout, the hole is a line from the last
    /// sample played, or zero, to the first that follows.
    #[test]
    fn with_too_little_history_the_hole_is_a_straight_line() {
        let mut row = [0; MAX_CHANNELS];
        row[0] = 100;
        let mut next = [0; MAX_CHANNELS];
        next[0] = 500;

        let mut concealer = Concealer::default();
        concealer.push(&[row; 10], 1);
        let mut gap = vec![[0; MAX_CHANNELS]; 3];
        concealer.fill(&mut gap, &[next; 40], 1);
        assert_eq!(
            gap.iter().map(|r| r[0]).collect::<Vec<_>>(),
            [200, 300, 400]
        );

        // History of another layout counts for nothing.
        concealer.fill(&mut gap, &[next; 40], 2);
        assert_eq!(
            gap.iter().map(|r| r[0]).collect::<Vec<_>>(),
            [125, 250, 375]
        );
    }

    #[test]
    fn rows_of_another_layout_start_the_history_over() {
        let mut concealer = Concealer::default();
        concealer.push(&music(0, 2000), 2);
        concealer.push(&[[1; MAX_CHANNELS]; 10], 6);
        assert_eq!(concealer.len, 10);
        assert_eq!(concealer.channels, 6);
    }

    /// After an overlap, the rows played next start where the kept rendering
    /// ended, and end where they would have.
    #[test]
    fn the_join_after_an_overlap_is_evened_out() {
        let mut kept = [0; MAX_CHANNELS];
        kept[0] = 1000;
        kept[1] = -1000;
        let mut dropped = [0; MAX_CHANNELS];
        dropped[0] = 1600;
        dropped[1] = -1000;

        let mut concealer = Concealer::default();
        concealer.push(&[kept; 40], 2);
        concealer.note_overlap(&[dropped; 40], 2);
        assert!(concealer.has_overlap());

        let mut next = vec![dropped; 40];
        concealer.smooth(&mut next, 2);
        assert!(!concealer.has_overlap());

        // Channel 0 starts next to the kept 1000 and ends at its own 1600.
        assert!((next[0][0] - 1000).abs() < 5, "{}", next[0][0]);
        assert!((next[39][0] - 1600).abs() < 5, "{}", next[39][0]);
        assert!(next.windows(2).all(|w| w[0][0] <= w[1][0]));
        // Channel 1 matched: untouched.
        assert!(next.iter().all(|r| r[1] == -1000));
    }

    #[test]
    fn a_bit_identical_overlap_leaves_nothing_to_smooth() {
        let rows = music(0, 80);
        let mut concealer = Concealer::default();
        concealer.push(&rows[..40], 2);
        concealer.note_overlap(&rows[..40], 2);
        assert!(!concealer.has_overlap());
    }
}
