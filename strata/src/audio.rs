//! Sound for models (feature `audio`): files decoded to one channel
//! ([`decode`]), resampled to a model's rate ([`resample`]), and the
//! pieces of a log-mel spectrogram ([`mel_filters`],
//! [`power_spectrogram`]) as the `transformers` library computes them.

use candle_core::{Device, Tensor};

use crate::embed::Audio;
use crate::error::{Error, Result};

/// The longest sound [`decode`] decodes, in seconds.
pub const MAX_SECONDS: usize = 3600;

/// The highest rate [`decode`] keeps; a file sampled faster is resampled
/// to it.
pub const MAX_RATE: u32 = 48_000;

fn bad(e: impl std::fmt::Display) -> Error {
    Error::Schema(format!("the audio could not be decoded: {e}"))
}

/// The sound of an audio file (WAV, FLAC, MP3, Ogg Vorbis, AAC in MP4) as
/// one channel, the mean of its channels, at its own rate (or
/// [`MAX_RATE`] if faster). A file longer than [`MAX_SECONDS`] is
/// refused.
pub fn decode(data: &[u8]) -> Result<Audio> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
    use symphonia::core::errors::Error as E;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(data.to_vec())), Default::default());
    let mut format = symphonia::default::get_probe().format(&Hint::new(), source, &FormatOptions::default(), &MetadataOptions::default()).map_err(bad)?.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or_else(|| bad("the file has no audio track"))?;
    let (id, rate) = (track.id, track.codec_params.sample_rate.ok_or_else(|| bad("the file does not say its sample rate"))?);
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(bad)?;
    let limit = rate as usize * MAX_SECONDS;
    let mut mono = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(E::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(bad(e)),
        };
        if packet.track_id() != id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A damaged packet is skipped, as players do.
            Err(E::DecodeError(_)) => continue,
            Err(e) => return Err(bad(e)),
        };
        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        mono.extend(buf.samples().chunks(channels).map(|f| f.iter().sum::<f32>() / channels as f32));
        if mono.len() > limit {
            return Err(Error::Schema(format!("the audio is longer than {} minutes", MAX_SECONDS / 60)));
        }
    }
    if rate > MAX_RATE {
        return Ok(Audio { rate: MAX_RATE, samples: resample(&mono, rate, MAX_RATE) });
    }
    Ok(Audio { rate, samples: mono })
}

/// `x`, sampled `from` times a second, sampled `to` times a second: a
/// windowed-sinc filter (Hann window, 24 zero crossings) that cuts below
/// the lower of the two Nyquist frequencies, its phases computed once.
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() || from == 0 || to == 0 {
        return x.to_vec();
    }
    let g = gcd(from as u64, to as u64);
    let (up, down) = (to as u64 / g, from as u64 / g);
    // Output n reads input position n * down / up: `up` phases repeat. A
    // rate with too many is read at the nearest of 4096 phases.
    let phases = up.min(4096) as usize;
    let ratio = to as f64 / from as f64;
    let cutoff = ratio.min(1.0) * 0.97;
    let half = (24.0 / cutoff).ceil() as i64;
    let taps = 2 * half as usize;
    let table: Vec<Vec<f32>> = (0..phases)
        .map(|p| {
            let frac = p as f64 / phases as f64;
            (0..taps)
                .map(|j| {
                    let d = frac + half as f64 - 1.0 - j as f64;
                    let u = d / half as f64;
                    let w = if u.abs() >= 1.0 { 0.0 } else { 0.5 * (1.0 + (std::f64::consts::PI * u).cos()) };
                    (cutoff * sinc(cutoff * d) * w) as f32
                })
                .collect()
        })
        .collect();
    let n = (x.len() as f64 * ratio).round() as usize;
    (0..n)
        .map(|i| {
            let pos = i as u64 * down;
            let c = (pos / up) as i64;
            let p = ((pos % up) * phases as u64 / up) as usize;
            let h = &table[p];
            let start = c - half + 1;
            let mut acc = 0f32;
            for (j, &w) in h.iter().enumerate() {
                let k = start + j as i64;
                if k >= 0 && (k as usize) < x.len() {
                    acc += x[k as usize] * w;
                }
            }
            acc
        })
        .collect()
}

fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        1.0
    } else {
        (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
    }
}

/// A mel filter bank as `librosa.filters.mel` makes it: `n_mels`
/// triangles over the `n_fft / 2 + 1` bins of an FFT at `rate`, from
/// `fmin` to `fmax` Hz, on Slaney's mel scale (HTK's with `htk`), each
/// scaled to unit area with `slaney_norm`. A row per filter.
pub fn mel_filters(rate: u32, n_fft: usize, n_mels: usize, fmin: f64, fmax: f64, htk: bool, slaney_norm: bool) -> Vec<Vec<f32>> {
    let to_mel = |f: f64| {
        if htk {
            2595.0 * (1.0 + f / 700.0).log10()
        } else if f < 1000.0 {
            3.0 * f / 200.0
        } else {
            15.0 + (f / 1000.0).ln() / (6.4f64.ln() / 27.0)
        }
    };
    let to_hz = |m: f64| {
        if htk {
            700.0 * (10f64.powf(m / 2595.0) - 1.0)
        } else if m < 15.0 {
            200.0 * m / 3.0
        } else {
            1000.0 * ((m - 15.0) * (6.4f64.ln() / 27.0)).exp()
        }
    };
    let bins = n_fft / 2 + 1;
    let freqs: Vec<f64> = (0..bins).map(|i| i as f64 * rate as f64 / n_fft as f64).collect();
    let (lo, hi) = (to_mel(fmin), to_mel(fmax));
    let edges: Vec<f64> = (0..n_mels + 2).map(|i| to_hz(lo + (hi - lo) * i as f64 / (n_mels + 1) as f64)).collect();
    (0..n_mels)
        .map(|m| {
            let (l, c, r) = (edges[m], edges[m + 1], edges[m + 2]);
            let norm = if slaney_norm { 2.0 / (r - l) } else { 1.0 };
            freqs.iter().map(|&f| ((f - l) / (c - l)).min((r - f) / (r - c)).max(0.0) * norm).map(|w| w as f32).collect()
        })
        .collect()
}

/// The power spectrogram of `x`, `(frames, n_fft / 2 + 1)`: frames
/// `hop` apart, centred (the sound padded by `n_fft / 2` on each side,
/// reflected), each under a periodic Hann window, as `torch.stft` with
/// `center=True` gives it. The transforms are a matrix product.
pub fn power_spectrogram(x: &[f32], n_fft: usize, hop: usize, dev: &Device) -> candle_core::Result<Tensor> {
    let pad = n_fft / 2;
    let at = |i: isize| -> f32 {
        let n = x.len() as isize;
        if n == 1 {
            return x[0];
        }
        // Reflect without repeating the edge sample, as numpy and torch do.
        let mut i = i;
        while i < 0 || i >= n {
            i = if i < 0 { -i } else { 2 * (n - 1) - i };
        }
        x[i as usize]
    };
    let frames = 1 + x.len() / hop;
    let window: Vec<f32> = (0..n_fft).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n_fft as f64).cos()) as f32).collect();
    let mut framed = Vec::with_capacity(frames * n_fft);
    for f in 0..frames {
        let start = (f * hop) as isize - pad as isize;
        framed.extend((0..n_fft).map(|j| at(start + j as isize) * window[j]));
    }
    let bins = n_fft / 2 + 1;
    let mut basis = Vec::with_capacity(n_fft * 2 * bins);
    for j in 0..n_fft {
        for k in 0..bins {
            let a = 2.0 * std::f64::consts::PI * (j * k % n_fft) as f64 / n_fft as f64;
            basis.push(a.cos() as f32);
        }
        for k in 0..bins {
            let a = 2.0 * std::f64::consts::PI * (j * k % n_fft) as f64 / n_fft as f64;
            basis.push(-(a.sin() as f32));
        }
    }
    let framed = Tensor::from_vec(framed, (frames, n_fft), dev)?;
    let basis = Tensor::from_vec(basis, (n_fft, 2 * bins), dev)?;
    let spec = framed.matmul(&basis)?;
    let (re, im) = (spec.narrow(1, 0, bins)?, spec.narrow(1, bins, bins)?);
    re.sqr()? + im.sqr()?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f64, rate: u32, seconds: f64) -> Vec<f32> {
        (0..(rate as f64 * seconds) as usize).map(|i| (2.0 * std::f64::consts::PI * hz * i as f64 / rate as f64).sin() as f32).collect()
    }

    #[test]
    fn resampling_keeps_a_tone_and_drops_what_the_new_rate_cannot_hold() {
        let x = tone(440.0, 44_100, 1.0);
        let y = resample(&x, 44_100, 16_000);
        assert_eq!(y.len(), 16_000);
        let want = tone(440.0, 16_000, 1.0);
        let err = y[200..15_800].iter().zip(&want[200..15_800]).map(|(a, b)| (a - b).abs()).fold(0f32, f32::max);
        assert!(err < 0.01, "a 440 Hz tone at 16 kHz is off by {err}");
        // 10 kHz is above 16 kHz's Nyquist frequency: it goes.
        let high = resample(&tone(10_000.0, 44_100, 1.0), 44_100, 16_000);
        let rms = (high[200..15_800].iter().map(|v| v * v).sum::<f32>() / 15_600.0).sqrt();
        assert!(rms < 0.01, "10 kHz kept at {rms}");
        // Up, and an odd ratio.
        assert_eq!(resample(&x, 44_100, 48_000).len(), 48_000);
        assert_eq!(resample(&tone(440.0, 22_050, 0.5), 22_050, 16_000).len(), 8_000);
        assert_eq!(resample(&x, 16_000, 16_000), x);
    }

    #[test]
    fn mel_filters_are_librosas() {
        // librosa.filters.mel(sr=16000, n_fft=400, n_mels=80): Whisper's.
        let f = mel_filters(16_000, 400, 80, 0.0, 8_000.0, false, true);
        assert_eq!((f.len(), f[0].len()), (80, 201));
        assert!((f[0][1] - 0.024_862_595).abs() < 1e-7, "{}", f[0][1]);
        assert!((f[79][199] - 0.000_448_759).abs() < 1e-8, "{}", f[79][199]);
        assert_eq!(f[79][200], 0.0);
        // Each filter is a triangle: up to one peak, then down.
        for row in &f {
            let peak = row.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
            assert!(row[..=peak].windows(2).all(|w| w[0] <= w[1]) && row[peak..].windows(2).all(|w| w[0] >= w[1]));
        }
    }

    #[test]
    fn a_tone_lights_its_bin() {
        let x = tone(1_000.0, 16_000, 0.5);
        let p = power_spectrogram(&x, 400, 160, &Device::Cpu).unwrap();
        assert_eq!(p.dims(), &[51, 201]);
        let row: Vec<f32> = p.get(25).unwrap().to_vec1().unwrap();
        let top = row.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0;
        assert_eq!(top, 25, "1 kHz is bin 25 of a 400-point FFT at 16 kHz");
    }
}
