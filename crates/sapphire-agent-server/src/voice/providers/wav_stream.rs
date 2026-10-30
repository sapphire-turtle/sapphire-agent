//! WAV-decode → mono → 16 kHz resample → 20 ms chunk streamer.
//!
//! Shared between every TTS provider that delivers a full WAV blob
//! at the end of synthesis (OpenAI TTS + any HTTP TTS that responds
//! with a WAV body). Keeps the per-provider code focused on the
//! network call.

use std::io::Cursor;

use tokio::sync::mpsc;

use crate::voice::PIPELINE_SAMPLE_RATE;

/// Decode a WAV blob, downmix to mono, resample to the pipeline rate,
/// and push 20 ms PCM chunks into `pcm_tx`. Backpressure on the
/// bounded mpsc paces the producer — no explicit sleep needed.
///
/// Errors only on WAV parse / sample-read failure. A `pcm_tx`
/// receiver drop is treated as caller cancellation and short-circuits
/// without returning an error.
pub async fn stream_wav(bytes: Vec<u8>, pcm_tx: mpsc::Sender<Vec<i16>>) -> anyhow::Result<()> {
    let (samples, sample_rate, channels) = tokio::task::spawn_blocking(move || decode_wav(&bytes))
        .await
        .map_err(|e| anyhow::anyhow!("WAV decode task panicked: {e}"))??;

    let mono = if channels == 1 {
        samples
    } else {
        samples
            .chunks(channels as usize)
            .map(|frame| {
                let sum: i32 = frame.iter().map(|s| *s as i32).sum();
                (sum / channels as i32) as i16
            })
            .collect()
    };

    let resampled = if sample_rate == PIPELINE_SAMPLE_RATE {
        mono
    } else {
        linear_resample(&mono, sample_rate, PIPELINE_SAMPLE_RATE)
    };

    let chunk_size = (PIPELINE_SAMPLE_RATE as usize) / 50;
    for chunk in resampled.chunks(chunk_size) {
        if pcm_tx.send(chunk.to_vec()).await.is_err() {
            break;
        }
    }
    Ok(())
}

/// Decode a WAV blob into interleaved i16 samples. Returns
/// `(samples, sample_rate, channels)`. Both 16-bit int and 32-bit
/// float WAVs are supported; bit-depths other than 16 are sample-
/// scaled to the i16 range.
pub fn decode_wav(bytes: &[u8]) -> anyhow::Result<(Vec<i16>, u32, u16)> {
    let mut reader = hound::WavReader::new(Cursor::new(bytes))
        .map_err(|e| anyhow::anyhow!("WAV header parse failed: {e}"))?;
    let spec = reader.spec();
    let samples: Vec<i16> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let bits = spec.bits_per_sample;
            reader
                .samples::<i32>()
                .map(|s| {
                    s.map(|v| {
                        if bits == 16 {
                            v as i16
                        } else {
                            let shift = (bits as i32) - 16;
                            if shift > 0 {
                                (v >> shift) as i16
                            } else {
                                (v << (-shift)) as i16
                            }
                        }
                    })
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| anyhow::anyhow!("WAV sample read failed: {e}"))?
        }
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .map(|s| s.map(|v| (v.clamp(-1.0, 1.0) * (i16::MAX as f32)) as i16))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| anyhow::anyhow!("WAV f32 sample read failed: {e}"))?,
    };
    Ok((samples, spec.sample_rate, spec.channels))
}

/// Linear-interpolation resampler. Adequate for speech.
pub fn linear_resample(input: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    if input.is_empty() || src_rate == dst_rate {
        return input.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let out_len = ((input.len() as f64) / ratio).round() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = src_pos - idx as f64;
        let s0 = input[idx.min(input.len() - 1)] as f64;
        let s1 = input[(idx + 1).min(input.len() - 1)] as f64;
        let v = s0 * (1.0 - frac) + s1 * frac;
        out.push(v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_downsample_halves_length_approximately() {
        let input: Vec<i16> = (0..32000).map(|i| (i % 1000) as i16).collect();
        let out = linear_resample(&input, 32000, 16000);
        assert!(
            (out.len() as isize - 16000).abs() <= 1,
            "got len {}",
            out.len()
        );
    }

    #[test]
    fn resample_no_op_when_rates_match() {
        let input: Vec<i16> = vec![100, 200, 300];
        let out = linear_resample(&input, 16000, 16000);
        assert_eq!(out, input);
    }
}
