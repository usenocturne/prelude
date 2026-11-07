use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use sonora::{
    AudioProcessing, Config, StreamConfig,
    config::{EchoCanceller as EchoCancellerConfig, HighPassFilter, NoiseSuppression},
};

const FRAMES_PER_SECOND: u32 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RenderFormat {
    sample_rate_hz: u32,
    channels: u16,
}

impl RenderFormat {
    fn frame_samples(self) -> usize {
        (self.sample_rate_hz / FRAMES_PER_SECOND) as usize * usize::from(self.channels)
    }

    fn stream(self) -> StreamConfig {
        StreamConfig::new(self.sample_rate_hz, self.channels)
    }
}

#[derive(Debug, Default)]
struct ReferenceQueue {
    listeners: usize,
    format: Option<RenderFormat>,
    samples: VecDeque<i16>,
}

#[derive(Debug, Clone, Default)]
pub struct EchoReference {
    queue: Arc<Mutex<ReferenceQueue>>,
}

impl EchoReference {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn render_s16le(&self, pcm_s16le: &[u8], sample_rate_hz: u32, channels: u16) {
        if sample_rate_hz < FRAMES_PER_SECOND || channels == 0 {
            return;
        }
        let mut queue = self.lock();
        if queue.listeners == 0 {
            return;
        }
        let format = RenderFormat {
            sample_rate_hz,
            channels,
        };
        if queue.format != Some(format) {
            queue.samples.clear();
            queue.format = Some(format);
        }
        queue.samples.extend(
            pcm_s16le
                .chunks_exact(2)
                .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]])),
        );
        let limit = sample_rate_hz as usize * usize::from(channels);
        let excess = queue.samples.len().saturating_sub(limit);
        queue.samples.drain(..excess);
    }

    #[cfg(test)]
    pub(crate) fn pending_samples(&self) -> usize {
        self.lock().samples.len()
    }

    fn lock(&self) -> MutexGuard<'_, ReferenceQueue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn attach(&self) {
        let mut queue = self.lock();
        queue.listeners += 1;
        queue.samples.clear();
    }

    fn detach(&self) {
        let mut queue = self.lock();
        queue.listeners = queue.listeners.saturating_sub(1);
        if queue.listeners == 0 {
            queue.samples.clear();
        }
    }

    fn take(&self, pending: &mut Vec<i16>) -> Option<RenderFormat> {
        let mut queue = self.lock();
        pending.extend(queue.samples.drain(..));
        queue.format
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EchoCancellerSummary {
    pub delay_ms: Option<i32>,
    pub echo_return_loss_enhancement_db: Option<f64>,
}

pub struct EchoCanceller {
    reference: EchoReference,
    processing: AudioProcessing,
    frame_samples: usize,
    render_format: Option<RenderFormat>,
    render_pending: Vec<i16>,
    render_output: Vec<i16>,
    near_end: Vec<i16>,
    processed: VecDeque<i16>,
    capture_output: Vec<i16>,
}

impl std::fmt::Debug for EchoCanceller {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EchoCanceller")
            .field("frame_samples", &self.frame_samples)
            .field("render_format", &self.render_format)
            .finish_non_exhaustive()
    }
}

impl EchoCanceller {
    pub fn attach(reference: &EchoReference, capture_rate_hz: u32) -> Self {
        let capture = StreamConfig::new(capture_rate_hz, 1);
        let processing = AudioProcessing::builder()
            .config(Config {
                high_pass_filter: Some(HighPassFilter::default()),
                echo_canceller: Some(EchoCancellerConfig::default()),
                noise_suppression: Some(NoiseSuppression::default()),
                ..Config::default()
            })
            .capture_config(capture)
            .render_config(capture)
            .build();
        let frame_samples = (capture_rate_hz / FRAMES_PER_SECOND) as usize;
        reference.attach();
        Self {
            reference: reference.clone(),
            processing,
            frame_samples,
            render_format: None,
            render_pending: Vec::new(),
            render_output: Vec::new(),
            near_end: Vec::with_capacity(frame_samples * 4),
            processed: VecDeque::from(vec![0; frame_samples]),
            capture_output: vec![0; frame_samples],
        }
    }

    pub fn process_s16le(&mut self, pcm_s16le: &mut [u8]) {
        if self.frame_samples == 0 {
            return;
        }
        self.near_end.extend(
            pcm_s16le
                .chunks_exact(2)
                .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]])),
        );
        let complete = self.near_end.len() - self.near_end.len() % self.frame_samples;
        for start in (0..complete).step_by(self.frame_samples) {
            self.release_render();
            let frame = &self.near_end[start..start + self.frame_samples];
            if self
                .processing
                .process_capture_i16(frame, &mut self.capture_output)
                .is_ok()
            {
                self.processed.extend(&self.capture_output);
            } else {
                self.processed.extend(frame);
            }
        }
        self.near_end.drain(..complete);
        for bytes in pcm_s16le.chunks_exact_mut(2) {
            let sample = self.processed.pop_front().unwrap_or(0);
            bytes.copy_from_slice(&sample.to_le_bytes());
        }
    }

    pub fn summary(&self) -> EchoCancellerSummary {
        let statistics = self.processing.statistics();
        EchoCancellerSummary {
            delay_ms: statistics.delay_ms,
            echo_return_loss_enhancement_db: statistics.echo_return_loss_enhancement,
        }
    }

    fn release_render(&mut self) {
        let format = self.reference.take(&mut self.render_pending);
        if format != self.render_format {
            self.render_format = format;
            if let Some(format) = format {
                self.render_output = vec![0; format.frame_samples()];
            }
        }
        let Some(format) = self.render_format else {
            self.render_pending.clear();
            return;
        };
        let frame_samples = format.frame_samples();
        let stream = format.stream();
        let complete = self.render_pending.len() - self.render_pending.len() % frame_samples;
        for frame in self.render_pending[..complete].chunks_exact(frame_samples) {
            let _ = self.processing.process_render_i16_with_config(
                frame,
                &stream,
                &stream,
                &mut self.render_output,
            );
        }
        self.render_pending.drain(..complete);
    }
}

impl Drop for EchoCanceller {
    fn drop(&mut self) {
        self.reference.detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIXER_RATE: u32 = 48_000;
    const CAPTURE_RATE: u32 = 48_000;

    struct Noise(u32);

    impl Noise {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            f64::from(self.0) / f64::from(u32::MAX) * 2.0 - 1.0
        }
    }

    fn encode(samples: &[i16]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    fn energy(samples: &[i16]) -> f64 {
        samples
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / samples.len().max(1) as f64
    }

    fn decibels(numerator: f64, denominator: f64) -> f64 {
        10.0 * (numerator / denominator.max(1.0)).log10()
    }

    fn run_call(
        echo_delay_ms: usize,
        render_chunk_ms: usize,
        capture_chunk_ms: usize,
    ) -> (Vec<i16>, Vec<i16>, EchoCancellerSummary) {
        let seconds = 12;
        let far_end_ms = 9_000;
        let driver_from_ms = 10_000;
        let mixer_per_ms = MIXER_RATE as usize / 1_000;
        let capture_per_ms = CAPTURE_RATE as usize / 1_000;
        let mut noise = Noise(0x9e37_79b9);
        let far_end: Vec<f64> = (0..seconds * 1_000 * capture_per_ms)
            .map(|index| {
                if index < far_end_ms * capture_per_ms {
                    noise.next() * 6_000.0
                } else {
                    0.0
                }
            })
            .collect();
        let echo_delay = echo_delay_ms * capture_per_ms;
        let near_end: Vec<i16> = (0..far_end.len())
            .map(|index| {
                let echo = [(0, 0.5), (37, 0.2), (113, -0.1)]
                    .iter()
                    .filter_map(|(tap, gain)| {
                        index
                            .checked_sub(echo_delay + tap)
                            .map(|source| far_end[source] * gain)
                    })
                    .sum::<f64>();
                let driver = if index >= driver_from_ms * capture_per_ms {
                    let time = index as f64 / f64::from(CAPTURE_RATE);
                    let voiced = (time * 140.0 * std::f64::consts::TAU).sin()
                        + 0.5 * (time * 280.0 * std::f64::consts::TAU).sin()
                        + 0.25 * (time * 420.0 * std::f64::consts::TAU).sin();
                    voiced * 2_500.0
                } else {
                    0.0
                };
                (echo + driver).clamp(-32_768.0, 32_767.0) as i16
            })
            .collect();
        let reference = EchoReference::new();
        let mut canceller = EchoCanceller::attach(&reference, CAPTURE_RATE);
        let mut uplink = Vec::with_capacity(near_end.len());
        let step_ms = 5;
        for now_ms in (0..seconds * 1_000).step_by(step_ms) {
            if now_ms % render_chunk_ms == 0 {
                let stereo: Vec<i16> = far_end
                    [now_ms * capture_per_ms..(now_ms + render_chunk_ms) * capture_per_ms]
                    .iter()
                    .flat_map(|sample| {
                        let sample = *sample as i16;
                        [sample, sample]
                    })
                    .collect();
                debug_assert_eq!(stereo.len(), render_chunk_ms * mixer_per_ms * 2);
                reference.render_s16le(&encode(&stereo), MIXER_RATE, 2);
            }
            if (now_ms + step_ms) % capture_chunk_ms == 0 {
                let start = (now_ms + step_ms - capture_chunk_ms) * capture_per_ms;
                let mut chunk = encode(&near_end[start..start + capture_chunk_ms * capture_per_ms]);
                canceller.process_s16le(&mut chunk);
                uplink.extend(
                    chunk
                        .chunks_exact(2)
                        .map(|bytes| i16::from_le_bytes([bytes[0], bytes[1]])),
                );
            }
        }
        let summary = canceller.summary();
        (near_end, uplink, summary)
    }

    fn assert_echo_removed_and_driver_kept(echo_delay_ms: usize, capture_chunk_ms: usize) {
        let (near_end, uplink, _) = run_call(echo_delay_ms, 10, capture_chunk_ms);
        let per_ms = CAPTURE_RATE as usize / 1_000;
        let shift = 10 * per_ms;
        let echo = 4_000 * per_ms..9_000 * per_ms;
        let driver = 10_500 * per_ms..12_000 * per_ms;
        let reduction = decibels(
            energy(&near_end[echo.clone()]),
            energy(&uplink[echo.start + shift..echo.end + shift]),
        );
        let kept = decibels(
            energy(&uplink[driver.start..driver.end]),
            energy(&near_end[driver.start - shift..driver.end - shift]),
        );
        assert!(reduction > 30.0, "echo only fell by {reduction:.1} dB");
        assert!(kept > -6.0, "driver fell by {kept:.1} dB");
    }

    #[test]
    fn cancels_the_car_speaker_echo_from_the_mixer_reference() {
        assert_echo_removed_and_driver_kept(180, 10);
    }

    #[test]
    fn cancels_echo_when_capture_buffers_are_not_whole_frames() {
        assert_echo_removed_and_driver_kept(250, 25);
    }

    #[test]
    fn keeps_buffer_sizes_and_delays_by_one_frame() {
        let reference = EchoReference::new();
        let mut canceller = EchoCanceller::attach(&reference, 24_000);
        let mut first = vec![0x7f_u8; 2 * 100];
        canceller.process_s16le(&mut first);
        assert_eq!(first.len(), 200);
        assert!(first.iter().all(|byte| *byte == 0));
    }

    #[test]
    fn ignores_the_mixer_when_no_microphone_is_listening() {
        let reference = EchoReference::new();
        reference.render_s16le(&[1, 0, 2, 0], MIXER_RATE, 2);
        assert!(reference.pending_samples() == 0);
        let canceller = EchoCanceller::attach(&reference, CAPTURE_RATE);
        reference.render_s16le(&[1, 0, 2, 0], MIXER_RATE, 2);
        assert_eq!(reference.pending_samples(), 2);
        drop(canceller);
        assert!(reference.pending_samples() == 0);
        reference.render_s16le(&[1, 0, 2, 0], MIXER_RATE, 2);
        assert!(reference.pending_samples() == 0);
    }

    #[test]
    fn bounds_the_reference_to_one_second() {
        let reference = EchoReference::new();
        let _canceller = EchoCanceller::attach(&reference, CAPTURE_RATE);
        let second = vec![0_u8; MIXER_RATE as usize * 2 * 2];
        reference.render_s16le(&second, MIXER_RATE, 2);
        reference.render_s16le(&second, MIXER_RATE, 2);
        assert_eq!(reference.pending_samples(), MIXER_RATE as usize * 2);
    }
}
