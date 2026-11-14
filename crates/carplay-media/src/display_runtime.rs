use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crate::{
    DurationStats, Error, Result, SCREEN_HEIGHT, SCREEN_WIDTH, wayland_presenter::WaylandPresenter,
};

const DISPLAY_POLL: Duration = Duration::from_millis(16);
const DISPLAY_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const WAITING_ANIMATION_FRAME_INTERVAL: Duration = Duration::from_millis(33);

#[derive(Debug, Default)]
struct DisplayState {
    video: Option<Arc<[u8]>>,
    overlay: Option<Arc<[u8]>>,
    startup_overlay: Option<Arc<[u8]>>,
    waiting_overlay: Option<Arc<[u8]>>,
    waiting_animation: Option<Arc<AtomicBool>>,
    projection_active: bool,
    host_ui_active: bool,
    video_sources: BTreeSet<u64>,
    source_frames: BTreeMap<u64, (u64, Arc<[u8]>)>,
    video_owner: Option<u64>,
    next_video_source: u64,
    next_video_frame: u64,
    generation: u64,
    stopped: bool,
    failure: Option<String>,
    video_submitted_at: Option<Instant>,
    video_commit: DurationStats,
}

#[derive(Debug, Default)]
struct SharedDisplay {
    state: Mutex<DisplayState>,
    changed: Condvar,
}

#[derive(Clone)]
pub struct LinuxDisplayHandle {
    shared: Arc<SharedDisplay>,
}

impl fmt::Debug for LinuxDisplayHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.shared.state.lock().ok();
        formatter
            .debug_struct("LinuxDisplayHandle")
            .field(
                "has_video",
                &state.as_ref().is_some_and(|state| state.video.is_some()),
            )
            .field(
                "has_overlay",
                &state.as_ref().is_some_and(|state| state.overlay.is_some()),
            )
            .field(
                "failed",
                &state.as_ref().is_none_or(|state| state.failure.is_some()),
            )
            .field(
                "projection_active",
                &state.as_ref().is_some_and(|state| state.projection_active),
            )
            .field(
                "host_ui_active",
                &state.as_ref().is_some_and(|state| state.host_ui_active),
            )
            .finish_non_exhaustive()
    }
}

impl LinuxDisplayHandle {
    pub fn submit_video(&self, frame: Vec<u8>) -> Result<()> {
        self.replace_frame(frame, false)
    }

    pub fn set_overlay(&self, frame: Vec<u8>) -> Result<()> {
        self.replace_frame(frame, true)
    }

    pub fn set_waiting_overlay(&self, frame: Vec<u8>) -> Result<()> {
        validate_frame(&frame)?;
        let frame = Arc::<[u8]>::from(frame);
        self.update(move |state| {
            cancel_waiting_animation(state);
            state.waiting_overlay = Some(frame);
        })
    }

    pub fn start_waiting_overlay_cycle(
        &self,
        frames: Vec<Vec<u8>>,
        interval: Duration,
        transition: Duration,
    ) -> Result<()> {
        if frames.is_empty() || interval.is_zero() || transition.is_zero() || transition > interval
        {
            return Err(Error::Backend(
                "waiting overlay cycle requires frames, a nonzero interval, and a transition no longer than the interval"
                    .to_owned(),
            ));
        }
        let frames = frames
            .into_iter()
            .map(|frame| {
                validate_frame(&frame)?;
                Ok::<Arc<[u8]>, Error>(Arc::from(frame))
            })
            .collect::<Result<Vec<_>>>()?;
        let cancellation = Arc::new(AtomicBool::new(false));
        {
            let mut state = self
                .shared
                .state
                .lock()
                .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
            ensure_display_running(&state)?;
            cancel_waiting_animation(&mut state);
            state.waiting_overlay = Some(Arc::clone(&frames[0]));
            state.waiting_animation = Some(Arc::clone(&cancellation));
            state.generation = state.generation.wrapping_add(1);
            self.shared.changed.notify_one();
        }

        let transition_steps = transition
            .as_millis()
            .div_ceil(WAITING_ANIMATION_FRAME_INTERVAL.as_millis())
            .clamp(1, u128::from(u32::MAX)) as u32;
        let transition_step = transition / transition_steps;
        let hold_interval = interval.saturating_sub(transition);
        let display = self.clone();
        let worker_cancellation = Arc::clone(&cancellation);
        if let Err(error) = thread::Builder::new()
            .name("prelude-waiting-overlay".to_owned())
            .spawn(move || {
                let mut frame_index = 0;
                while !worker_cancellation.load(Ordering::Acquire) {
                    thread::sleep(hold_interval);
                    let next_frame_index = (frame_index + 1) % frames.len();
                    for step in 1..=transition_steps {
                        thread::sleep(transition_step);
                        if worker_cancellation.load(Ordering::Acquire) {
                            return;
                        }
                        let blended = Arc::<[u8]>::from(blend_frames(
                            &frames[frame_index],
                            &frames[next_frame_index],
                            step,
                            transition_steps,
                        ));
                        let Ok(mut state) = display.shared.state.lock() else {
                            return;
                        };
                        let Some(active) = state.waiting_animation.as_ref() else {
                            return;
                        };
                        if !Arc::ptr_eq(active, &worker_cancellation) || state.stopped {
                            return;
                        }
                        state.waiting_overlay = Some(blended);
                        state.generation = state.generation.wrapping_add(1);
                        display.shared.changed.notify_one();
                    }
                    frame_index = next_frame_index;
                }
            })
        {
            cancellation.store(true, Ordering::Release);
            if let Ok(mut state) = self.shared.state.lock() {
                if state
                    .waiting_animation
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(active, &cancellation))
                {
                    state.waiting_animation = None;
                }
            }
            return Err(Error::Backend(format!(
                "cannot start waiting overlay animation: {error}"
            )));
        }
        Ok(())
    }

    pub fn clear_video(&self) -> Result<()> {
        self.update(|state| {
            state.video = None;
            state.video_owner = None;
            state.source_frames.clear();
        })
    }

    pub fn clear_overlay(&self) -> Result<()> {
        self.update(|state| state.overlay = None)
    }

    pub fn clear_waiting_overlay(&self) -> Result<()> {
        self.update(|state| {
            cancel_waiting_animation(state);
            state.waiting_overlay = None;
        })
    }

    pub fn set_projection_active(&self, active: bool) -> Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        ensure_display_running(&state)?;
        if state.projection_active == active {
            return Ok(());
        }
        state.projection_active = active;
        state.generation = state.generation.wrapping_add(1);
        self.shared.changed.notify_one();
        Ok(())
    }

    pub fn projection_active(&self) -> bool {
        self.shared
            .state
            .lock()
            .map_or(true, |state| state.projection_active)
    }

    pub fn set_host_ui_active(&self, active: bool) -> Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        ensure_display_running(&state)?;
        if state.host_ui_active == active {
            return Ok(());
        }
        state.host_ui_active = active;
        state.generation = state.generation.wrapping_add(1);
        self.shared.changed.notify_one();
        Ok(())
    }

    pub fn host_ui_active(&self) -> bool {
        self.shared
            .state
            .lock()
            .is_ok_and(|state| state.host_ui_active)
    }

    pub fn check(&self) -> Result<()> {
        let state = self
            .shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        if let Some(failure) = &state.failure {
            return Err(Error::Backend(failure.clone()));
        }
        if state.stopped {
            return Err(Error::Backend("display runtime has stopped".to_owned()));
        }
        Ok(())
    }

    pub(crate) fn submit_video_for_source(&self, source: u64, frame: Vec<u8>) -> Result<()> {
        validate_frame(&frame)?;
        let frame = Arc::<[u8]>::from(frame);
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        ensure_display_running(&state)?;
        if !state.video_sources.contains(&source) {
            return Err(Error::Backend(
                "display video source is no longer registered".to_owned(),
            ));
        }
        let frame_sequence = state.next_video_frame;
        state.next_video_frame = frame_sequence
            .checked_add(1)
            .ok_or_else(|| Error::Backend("display video-frame sequence overflowed".to_owned()))?;
        state.video = Some(Arc::clone(&frame));
        state.video_owner = Some(source);
        state.source_frames.insert(source, (frame_sequence, frame));
        state.video_submitted_at.get_or_insert_with(Instant::now);
        state.generation = state.generation.wrapping_add(1);
        self.shared.changed.notify_one();
        Ok(())
    }

    pub fn take_video_commit_stats(&self) -> DurationStats {
        self.shared
            .state
            .lock()
            .map(|mut state| std::mem::take(&mut state.video_commit))
            .unwrap_or_default()
    }

    pub(crate) fn register_video_source(&self) -> Result<u64> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        ensure_display_running(&state)?;
        let source = state.next_video_source;
        state.next_video_source = source
            .checked_add(1)
            .ok_or_else(|| Error::Backend("display video-source ID overflowed".to_owned()))?;
        if !state.video_sources.insert(source) {
            return Err(Error::Backend(
                "display video-source ID was reused".to_owned(),
            ));
        }
        Ok(source)
    }

    pub(crate) fn unregister_video_source(&self, source: u64) -> Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        ensure_display_running(&state)?;
        if !state.video_sources.remove(&source) {
            return Err(Error::Backend(
                "display video source was not registered".to_owned(),
            ));
        }
        state.source_frames.remove(&source);
        if state.video_owner == Some(source) {
            let replacement = state
                .source_frames
                .iter()
                .max_by_key(|(_, (sequence, _))| sequence)
                .map(|(source, (_, frame))| (*source, Arc::clone(frame)));
            if let Some((source, frame)) = replacement {
                state.video = Some(frame);
                state.video_owner = Some(source);
            } else {
                state.video = None;
                state.video_owner = None;
            }
            state.generation = state.generation.wrapping_add(1);
            self.shared.changed.notify_one();
        }
        Ok(())
    }

    fn replace_frame(&self, frame: Vec<u8>, overlay: bool) -> Result<()> {
        validate_frame(&frame)?;
        let frame = Arc::<[u8]>::from(frame);
        self.update(move |state| {
            if overlay {
                state.overlay = Some(frame);
            } else {
                state.video = Some(frame);
                state.video_owner = None;
            }
        })
    }

    fn update(&self, update: impl FnOnce(&mut DisplayState)) -> Result<()> {
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        ensure_display_running(&state)?;
        update(&mut state);
        state.generation = state.generation.wrapping_add(1);
        self.shared.changed.notify_one();
        Ok(())
    }
}

pub struct LinuxDisplayRuntime {
    handle: LinuxDisplayHandle,
    worker: Option<JoinHandle<Result<()>>>,
}

impl fmt::Debug for LinuxDisplayRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LinuxDisplayRuntime")
            .field("handle", &self.handle)
            .field("worker_running", &self.worker.is_some())
            .finish()
    }
}

impl LinuxDisplayRuntime {
    pub fn start() -> Result<Self> {
        Self::start_with_initial_overlay(None)
    }

    pub fn start_with_overlay(frame: Vec<u8>) -> Result<Self> {
        validate_frame(&frame)?;
        Self::start_with_initial_overlay(Some(Arc::from(frame)))
    }

    fn start_with_initial_overlay(initial_overlay: Option<Arc<[u8]>>) -> Result<Self> {
        let generation = u64::from(initial_overlay.is_some());
        let shared = Arc::new(SharedDisplay {
            state: Mutex::new(DisplayState {
                startup_overlay: initial_overlay,
                generation,
                ..DisplayState::default()
            }),
            changed: Condvar::new(),
        });
        let worker_shared = Arc::clone(&shared);
        let (startup_tx, startup_rx) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("prelude-display".to_owned())
            .spawn(move || run_display(worker_shared, startup_tx))
            .map_err(|error| Error::Backend(format!("cannot start display runtime: {error}")))?;
        match startup_rx.recv_timeout(DISPLAY_STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                handle: LinuxDisplayHandle { shared },
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                drop(worker);
                Err(error)
            }
            Err(_) => {
                if let Ok(mut state) = shared.state.lock() {
                    state.stopped = true;
                    shared.changed.notify_all();
                }
                drop(worker);
                Err(Error::Backend(
                    "display runtime did not start before its deadline".to_owned(),
                ))
            }
        }
    }

    pub fn handle(&self) -> LinuxDisplayHandle {
        self.handle.clone()
    }

    pub fn shutdown(mut self) -> Result<()> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<()> {
        if let Ok(mut state) = self.handle.shared.state.lock() {
            state.stopped = true;
            self.handle.shared.changed.notify_all();
        }
        let Some(worker) = self.worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| Error::Backend("display runtime thread panicked".to_owned()))?
    }
}

impl Drop for LinuxDisplayRuntime {
    fn drop(&mut self) {
        let _ = self.stop_and_join();
    }
}

fn run_display(shared: Arc<SharedDisplay>, startup: mpsc::SyncSender<Result<()>>) -> Result<()> {
    let mut presenter =
        match WaylandPresenter::connect(usize::from(SCREEN_WIDTH), usize::from(SCREEN_HEIGHT)) {
            Ok(presenter) => presenter,
            Err(detail) => {
                let error = Error::Backend(detail);
                let _ = startup.send(Err(Error::Backend(error.to_string())));
                return Err(error);
            }
        };
    let (initial_frame, mut rendered_generation, stopped) = {
        let state = shared
            .state
            .lock()
            .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
        (visible_frame(&state), state.generation, state.stopped)
    };
    if stopped {
        return Ok(());
    }
    if let Some(frame) = initial_frame {
        if let Err(detail) = presenter.present_packed_nv12(&frame) {
            let error = Error::Backend(detail);
            let _ = startup.send(Err(Error::Backend(error.to_string())));
            return Err(error);
        }
    }
    let _ = startup.send(Ok(()));
    loop {
        let (frame, generation, stopped, video_submitted_at) = {
            let state = shared
                .state
                .lock()
                .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
            let (mut state, _) = shared
                .changed
                .wait_timeout_while(state, DISPLAY_POLL, |state| {
                    !state.stopped && state.generation == rendered_generation
                })
                .map_err(|_| Error::Backend("display state lock is poisoned".to_owned()))?;
            (
                visible_frame(&state),
                state.generation,
                state.stopped,
                state.video_submitted_at.take(),
            )
        };
        if stopped {
            return Ok(());
        }
        let result = if generation != rendered_generation {
            if let Some(frame) = frame {
                presenter.present_packed_nv12(&frame)
            } else {
                presenter.clear()
            }
        } else {
            presenter.pump()
        };
        if let Err(detail) = result {
            if let Ok(mut state) = shared.state.lock() {
                state.failure = Some(detail.clone());
                state.stopped = true;
                shared.changed.notify_all();
            }
            return Err(Error::Backend(detail));
        }
        if let Some(submitted_at) = video_submitted_at
            && let Ok(mut state) = shared.state.lock()
        {
            state.video_commit.record(submitted_at.elapsed());
        }
        rendered_generation = generation;
    }
}

fn ensure_display_running(state: &DisplayState) -> Result<()> {
    if let Some(failure) = &state.failure {
        return Err(Error::Backend(failure.clone()));
    }
    if state.stopped {
        return Err(Error::Backend("display runtime has stopped".to_owned()));
    }
    Ok(())
}

fn cancel_waiting_animation(state: &mut DisplayState) {
    if let Some(cancellation) = state.waiting_animation.take() {
        cancellation.store(true, Ordering::Release);
    }
}

fn blend_frames(first: &[u8], second: &[u8], step: u32, total_steps: u32) -> Vec<u8> {
    let first_weight = total_steps - step;
    first
        .iter()
        .zip(second)
        .map(|(&first, &second)| {
            let value = u32::from(first) * first_weight + u32::from(second) * step;
            u8::try_from((value + total_steps / 2) / total_steps).unwrap_or(u8::MAX)
        })
        .collect()
}

fn visible_frame(state: &DisplayState) -> Option<Arc<[u8]>> {
    state
        .overlay
        .clone()
        .or_else(|| state.video.clone().filter(|_| state.projection_active))
        .or_else(|| state.waiting_overlay.clone())
        .or_else(|| state.startup_overlay.clone())
}

fn validate_frame(frame: &[u8]) -> Result<()> {
    let expected = usize::from(SCREEN_WIDTH)
        .checked_mul(usize::from(SCREEN_HEIGHT))
        .and_then(|pixels| pixels.checked_mul(3))
        .map(|bytes| bytes / 2)
        .ok_or_else(|| Error::Backend("display frame size overflowed".to_owned()))?;
    if frame.len() != expected {
        return Err(Error::Backend(format!(
            "display frame is {} bytes, expected {expected}",
            frame.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_handle_rejects_wrong_frame_size_without_mutation() {
        let shared = Arc::new(SharedDisplay::default());
        let handle = LinuxDisplayHandle {
            shared: Arc::clone(&shared),
        };
        assert!(handle.submit_video(vec![0; 16]).is_err());
        let state = shared.state.lock().expect("state");
        assert!(state.video.is_none());
        assert_eq!(state.generation, 0);
    }

    #[test]
    fn overlay_has_priority_and_clear_reveals_latest_video() {
        let shared = Arc::new(SharedDisplay::default());
        let handle = LinuxDisplayHandle {
            shared: Arc::clone(&shared),
        };
        let size = usize::from(SCREEN_WIDTH) * usize::from(SCREEN_HEIGHT) * 3 / 2;
        handle.submit_video(vec![1; size]).expect("video");
        handle.set_overlay(vec![2; size]).expect("overlay");
        {
            let state = shared.state.lock().expect("state");
            assert_eq!(state.overlay.as_deref().map(|frame| frame[0]), Some(2));
            assert_eq!(state.video.as_deref().map(|frame| frame[0]), Some(1));
        }
        handle.clear_overlay().expect("clear overlay");
        let state = shared.state.lock().expect("state");
        assert!(state.overlay.is_none());
        assert_eq!(state.video.as_deref().map(|frame| frame[0]), Some(1));
    }

    #[test]
    fn host_ui_and_projection_can_be_active_together() {
        let shared = Arc::new(SharedDisplay::default());
        let handle = LinuxDisplayHandle { shared };
        assert!(!handle.host_ui_active());
        assert!(!handle.projection_active());
        handle
            .set_projection_active(true)
            .expect("projection active");
        handle.set_host_ui_active(true).expect("host UI active");
        assert!(handle.host_ui_active());
        assert!(handle.projection_active());
        handle
            .set_projection_active(false)
            .expect("projection inactive");
        assert!(handle.host_ui_active());
        handle.set_host_ui_active(false).expect("host UI inactive");
        assert!(!handle.host_ui_active());
    }

    #[test]
    fn waiting_overlay_is_below_video_and_above_startup_overlay() {
        let size = usize::from(SCREEN_WIDTH) * usize::from(SCREEN_HEIGHT) * 3 / 2;
        let shared = Arc::new(SharedDisplay {
            state: Mutex::new(DisplayState {
                startup_overlay: Some(Arc::from(vec![1; size])),
                ..DisplayState::default()
            }),
            changed: Condvar::new(),
        });
        let handle = LinuxDisplayHandle { shared };
        handle
            .set_projection_active(true)
            .expect("projection active");

        handle
            .set_waiting_overlay(vec![2; size])
            .expect("waiting overlay");
        assert_eq!(
            visible_frame(&handle.shared.state.lock().expect("state"))
                .as_deref()
                .map(|frame| frame[0]),
            Some(2)
        );

        handle.submit_video(vec![3; size]).expect("video");
        assert_eq!(
            visible_frame(&handle.shared.state.lock().expect("state"))
                .as_deref()
                .map(|frame| frame[0]),
            Some(3)
        );

        handle.clear_video().expect("clear video");
        assert_eq!(
            visible_frame(&handle.shared.state.lock().expect("state"))
                .as_deref()
                .map(|frame| frame[0]),
            Some(2)
        );

        handle
            .clear_waiting_overlay()
            .expect("clear waiting overlay");
        assert_eq!(
            visible_frame(&handle.shared.state.lock().expect("state"))
                .as_deref()
                .map(|frame| frame[0]),
            Some(1)
        );
    }

    #[test]
    fn a_lost_phone_hides_its_last_video_frame() {
        let size = usize::from(SCREEN_WIDTH) * usize::from(SCREEN_HEIGHT) * 3 / 2;
        let shared = Arc::new(SharedDisplay::default());
        let handle = LinuxDisplayHandle { shared };
        handle
            .set_waiting_overlay(vec![1; size])
            .expect("waiting overlay");
        handle
            .set_projection_active(true)
            .expect("projection active");
        let source = handle.register_video_source().expect("source");
        handle
            .submit_video_for_source(source, vec![2; size])
            .expect("video");
        assert_eq!(
            visible_frame(&handle.shared.state.lock().expect("state"))
                .as_deref()
                .map(|frame| frame[0]),
            Some(2)
        );

        handle
            .set_projection_active(false)
            .expect("projection inactive");
        assert_eq!(
            visible_frame(&handle.shared.state.lock().expect("state"))
                .as_deref()
                .map(|frame| frame[0]),
            Some(1)
        );
    }

    #[test]
    fn waiting_overlay_cycle_replaces_frames_until_selected_overlay_cancels_it() {
        let size = usize::from(SCREEN_WIDTH) * usize::from(SCREEN_HEIGHT) * 3 / 2;
        let shared = Arc::new(SharedDisplay::default());
        let handle = LinuxDisplayHandle { shared };
        handle
            .start_waiting_overlay_cycle(
                vec![vec![1; size], vec![2; size]],
                Duration::from_millis(20),
                Duration::from_millis(10),
            )
            .expect("start waiting overlay cycle");
        thread::sleep(Duration::from_millis(10));
        let frame = visible_frame(&handle.shared.state.lock().expect("state"))
            .as_deref()
            .map(|frame| frame[0]);
        assert!(matches!(frame, Some(1) | Some(2)));

        handle
            .set_waiting_overlay(vec![3; size])
            .expect("select waiting overlay");
        thread::sleep(Duration::from_millis(5));
        let state = handle.shared.state.lock().expect("state");
        assert_eq!(
            visible_frame(&state).as_deref().map(|frame| frame[0]),
            Some(3)
        );
        assert!(state.waiting_animation.is_none());
    }

    #[test]
    fn waiting_overlay_blend_interpolates_frame_bytes() {
        assert_eq!(blend_frames(&[0, 10], &[100, 110], 1, 2), vec![50, 60]);
    }

    #[test]
    fn video_sources_restore_waiting_overlay_after_the_last_disconnect() {
        let size = usize::from(SCREEN_WIDTH) * usize::from(SCREEN_HEIGHT) * 3 / 2;
        let startup = Arc::<[u8]>::from(vec![2; size]);
        let shared = Arc::new(SharedDisplay {
            state: Mutex::new(DisplayState {
                startup_overlay: Some(startup),
                generation: 1,
                ..DisplayState::default()
            }),
            changed: Condvar::new(),
        });
        let handle = LinuxDisplayHandle { shared };
        handle
            .set_projection_active(true)
            .expect("projection active");
        handle
            .set_waiting_overlay(vec![5; size])
            .expect("waiting overlay");

        let first = handle.register_video_source().expect("first source");
        let second = handle.register_video_source().expect("second source");
        handle
            .submit_video_for_source(first, vec![1; size])
            .expect("first video");
        handle
            .submit_video_for_source(second, vec![3; size])
            .expect("second video");
        handle
            .submit_video_for_source(first, vec![4; size])
            .expect("newest first-source video");
        {
            let state = handle.shared.state.lock().expect("state");
            assert_eq!(
                visible_frame(&state).as_deref().map(|frame| frame[0]),
                Some(4)
            );
        }

        handle
            .unregister_video_source(first)
            .expect("first disconnect");
        {
            let state = handle.shared.state.lock().expect("state");
            assert_eq!(
                visible_frame(&state).as_deref().map(|frame| frame[0]),
                Some(3)
            );
        }

        handle
            .unregister_video_source(second)
            .expect("last disconnect");
        let state = handle.shared.state.lock().expect("state");
        assert!(state.video.is_none());
        assert_eq!(
            visible_frame(&state).as_deref().map(|frame| frame[0]),
            Some(5)
        );
        drop(state);
        handle
            .clear_waiting_overlay()
            .expect("clear waiting overlay");
        let state = handle.shared.state.lock().expect("state");
        assert_eq!(
            visible_frame(&state).as_deref().map(|frame| frame[0]),
            Some(2)
        );
    }
}
