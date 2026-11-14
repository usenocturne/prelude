use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAXIMUM_TRACKED_FRAMES: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DurationStats {
    pub count: u64,
    pub total_micros: u64,
    pub minimum_micros: u64,
    pub maximum_micros: u64,
}

impl DurationStats {
    pub fn record(&mut self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        self.minimum_micros = if self.count == 0 {
            micros
        } else {
            self.minimum_micros.min(micros)
        };
        self.maximum_micros = self.maximum_micros.max(micros);
        self.total_micros = self.total_micros.saturating_add(micros);
        self.count = self.count.saturating_add(1);
    }

    pub const fn average_micros(&self) -> Option<u64> {
        self.total_micros.checked_div(self.count)
    }

    pub const fn minimum(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.minimum_micros)
        }
    }

    pub const fn maximum(&self) -> Option<u64> {
        if self.count == 0 {
            None
        } else {
            Some(self.maximum_micros)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoLatencyWindow {
    pub pushed_frames: u64,
    pub decoded_frames: u64,
    pub rendered_frames: u64,
    pub skipped_frames: u64,
    pub unmatched_outputs: u64,
    pub pipeline_restarts: u64,
    pub flush_pictures: u64,
    pub ingress_wait: DurationStats,
    pub decode: DurationStats,
    pub render: DurationStats,
    pub arrival_to_display: DurationStats,
    pub held_frames_minimum: Option<u64>,
    pub held_frames_maximum: Option<u64>,
    pub in_flight_frames: u64,
    pub oldest_in_flight_micros: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct DecodedFrame {
    arrived_at: Option<Instant>,
    decoded_at: Instant,
}

#[derive(Debug)]
struct InFlight {
    presentation_time_ns: u64,
    sequence: u64,
    arrived_at: Option<Instant>,
    pushed_at: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    arrivals: VecDeque<(u64, Instant)>,
    in_flight: VecDeque<InFlight>,
    pushed: u64,
    window: VideoLatencyWindow,
}

#[derive(Debug, Clone, Default)]
pub struct VideoLatencyProbe {
    inner: Arc<Mutex<Inner>>,
}

impl VideoLatencyProbe {
    pub fn record_arrival(&self, frame_identifier: u64, arrived_at: Instant) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.arrivals.len() >= MAXIMUM_TRACKED_FRAMES {
            inner.arrivals.pop_front();
        }
        inner.arrivals.push_back((frame_identifier, arrived_at));
    }

    pub fn record_push(
        &self,
        frame_identifier: u64,
        presentation_time_ns: u64,
        pushed_at: Instant,
    ) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let mut arrived_at = None;
        while let Some(&(identifier, arrival)) = inner.arrivals.front() {
            if identifier > frame_identifier {
                break;
            }
            inner.arrivals.pop_front();
            if identifier == frame_identifier {
                arrived_at = Some(arrival);
                break;
            }
        }
        if let Some(arrived_at) = arrived_at {
            inner
                .window
                .ingress_wait
                .record(pushed_at.saturating_duration_since(arrived_at));
        }
        inner.pushed = inner.pushed.saturating_add(1);
        inner.window.pushed_frames = inner.window.pushed_frames.saturating_add(1);
        let sequence = inner.pushed;
        if inner.in_flight.len() >= MAXIMUM_TRACKED_FRAMES {
            inner.in_flight.pop_front();
            inner.window.skipped_frames = inner.window.skipped_frames.saturating_add(1);
        }
        inner.in_flight.push_back(InFlight {
            presentation_time_ns,
            sequence,
            arrived_at,
            pushed_at,
        });
    }

    pub fn record_output(&self, presentation_time_ns: u64, decoded_at: Instant) -> DecodedFrame {
        let unmatched = DecodedFrame {
            arrived_at: None,
            decoded_at,
        };
        let Ok(mut inner) = self.inner.lock() else {
            return unmatched;
        };
        let Some(position) = inner
            .in_flight
            .iter()
            .position(|frame| frame.presentation_time_ns == presentation_time_ns)
        else {
            inner.window.unmatched_outputs = inner.window.unmatched_outputs.saturating_add(1);
            return unmatched;
        };
        inner.window.skipped_frames = inner.window.skipped_frames.saturating_add(position as u64);
        inner.in_flight.drain(..position);
        let Some(frame) = inner.in_flight.pop_front() else {
            return unmatched;
        };
        let held = inner.pushed.saturating_sub(frame.sequence);
        let window = &mut inner.window;
        window.decoded_frames = window.decoded_frames.saturating_add(1);
        window
            .decode
            .record(decoded_at.saturating_duration_since(frame.pushed_at));
        window.held_frames_minimum = Some(window.held_frames_minimum.map_or(held, |v| v.min(held)));
        window.held_frames_maximum = Some(window.held_frames_maximum.map_or(held, |v| v.max(held)));
        DecodedFrame {
            arrived_at: frame.arrived_at,
            decoded_at,
        }
    }

    pub fn record_render(&self, frame: DecodedFrame, rendered_at: Instant) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let window = &mut inner.window;
        window.rendered_frames = window.rendered_frames.saturating_add(1);
        window
            .render
            .record(rendered_at.saturating_duration_since(frame.decoded_at));
        if let Some(arrived_at) = frame.arrived_at {
            window
                .arrival_to_display
                .record(rendered_at.saturating_duration_since(arrived_at));
        }
    }

    pub fn record_flush_pictures(&self, count: usize) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.window.flush_pictures = inner.window.flush_pictures.saturating_add(count as u64);
    }

    pub fn record_pipeline_restart(&self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let discarded = inner.in_flight.len() as u64;
        inner.in_flight.clear();
        inner.window.skipped_frames = inner.window.skipped_frames.saturating_add(discarded);
        inner.window.pipeline_restarts = inner.window.pipeline_restarts.saturating_add(1);
    }

    pub fn take_window(&self, now: Instant) -> VideoLatencyWindow {
        let Ok(mut inner) = self.inner.lock() else {
            return VideoLatencyWindow::default();
        };
        let mut window = std::mem::take(&mut inner.window);
        window.in_flight_frames = inner.in_flight.len() as u64;
        window.oldest_in_flight_micros = inner.in_flight.front().map(|frame| {
            u64::try_from(now.saturating_duration_since(frame.pushed_at).as_micros())
                .unwrap_or(u64::MAX)
        });
        window
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, millis: u64) -> Instant {
        start + Duration::from_millis(millis)
    }

    #[test]
    fn immediate_decoder_output_reports_no_held_frames() {
        let start = Instant::now();
        let probe = VideoLatencyProbe::default();
        for frame in 1..=3_u64 {
            let base = frame * 100;
            probe.record_arrival(frame, at(start, base));
            probe.record_push(frame, frame * 1_000, at(start, base + 2));
            let decoded = probe.record_output(frame * 1_000, at(start, base + 12));
            probe.record_render(decoded, at(start, base + 15));
        }
        let window = probe.take_window(at(start, 400));
        assert_eq!(window.pushed_frames, 3);
        assert_eq!(window.decoded_frames, 3);
        assert_eq!(window.rendered_frames, 3);
        assert_eq!(window.held_frames_minimum, Some(0));
        assert_eq!(window.held_frames_maximum, Some(0));
        assert_eq!(window.ingress_wait.average_micros(), Some(2_000));
        assert_eq!(window.decode.average_micros(), Some(10_000));
        assert_eq!(window.render.average_micros(), Some(3_000));
        assert_eq!(window.arrival_to_display.maximum(), Some(15_000));
        assert_eq!(window.in_flight_frames, 0);
        assert_eq!(window.oldest_in_flight_micros, None);
    }

    #[test]
    fn decoder_that_waits_for_the_next_frame_reports_one_held_frame() {
        let start = Instant::now();
        let probe = VideoLatencyProbe::default();
        probe.record_arrival(1, at(start, 0));
        probe.record_push(1, 1_000, at(start, 1));
        probe.record_arrival(2, at(start, 100));
        probe.record_push(2, 2_000, at(start, 101));
        let first = probe.record_output(1_000, at(start, 110));
        probe.record_render(first, at(start, 112));
        probe.record_arrival(3, at(start, 200));
        probe.record_push(3, 3_000, at(start, 201));
        let second = probe.record_output(2_000, at(start, 210));
        probe.record_render(second, at(start, 212));

        let window = probe.take_window(at(start, 500));
        assert_eq!(window.held_frames_minimum, Some(1));
        assert_eq!(window.held_frames_maximum, Some(1));
        assert_eq!(window.decode.minimum(), Some(109_000));
        assert_eq!(window.arrival_to_display.maximum(), Some(112_000));
        assert_eq!(window.in_flight_frames, 1);
        assert_eq!(window.oldest_in_flight_micros, Some(299_000));
    }

    #[test]
    fn dropped_and_unknown_outputs_are_counted_separately() {
        let start = Instant::now();
        let probe = VideoLatencyProbe::default();
        for frame in 1..=3_u64 {
            probe.record_push(frame, frame * 1_000, at(start, frame));
        }
        let _ = probe.record_output(3_000, at(start, 10));
        let _ = probe.record_output(9_000, at(start, 11));
        let window = probe.take_window(at(start, 20));
        assert_eq!(window.decoded_frames, 1);
        assert_eq!(window.skipped_frames, 2);
        assert_eq!(window.unmatched_outputs, 1);
        assert_eq!(window.ingress_wait.count, 0);
    }

    #[test]
    fn pipeline_restart_discards_frames_in_flight_and_window_resets() {
        let start = Instant::now();
        let probe = VideoLatencyProbe::default();
        probe.record_push(1, 1_000, at(start, 1));
        probe.record_push(2, 2_000, at(start, 2));
        probe.record_flush_pictures(3);
        probe.record_pipeline_restart();
        let window = probe.take_window(at(start, 3));
        assert_eq!(window.flush_pictures, 3);
        assert_eq!(window.pipeline_restarts, 1);
        assert_eq!(window.skipped_frames, 2);
        assert_eq!(window.in_flight_frames, 0);
        assert_eq!(
            probe.take_window(at(start, 4)),
            VideoLatencyWindow::default()
        );
    }
}
