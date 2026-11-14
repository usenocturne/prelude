use std::fmt;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_output, delegate_registry, delegate_shm, delegate_xdg_shell,
    delegate_xdg_window,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        xdg::{
            XdgShell,
            window::{Window, WindowConfigure, WindowDecorations, WindowHandler},
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use wayland_client::{
    Connection, EventQueue, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm, wl_surface},
};

const NV12_PLANE_RATIO_NUMERATOR: usize = 3;
const NV12_PLANE_RATIO_DENOMINATOR: usize = 2;
const BUFFER_POOL_SIZE: usize = 6;

pub(crate) struct WaylandPresenter {
    connection: Connection,
    event_queue: EventQueue<PresenterState>,
    state: PresenterState,
}

impl fmt::Debug for WaylandPresenter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WaylandPresenter")
            .field("width", &self.state.width)
            .field("height", &self.state.height)
            .field("configured", &self.state.configured)
            .field("closed", &self.state.closed)
            .finish_non_exhaustive()
    }
}

impl WaylandPresenter {
    pub(crate) fn connect(width: usize, height: usize) -> Result<Self, String> {
        let width_u32 = u32::try_from(width).map_err(|_| "Wayland width is too large")?;
        let height_u32 = u32::try_from(height).map_err(|_| "Wayland height is too large")?;
        let frame_size = width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(NV12_PLANE_RATIO_NUMERATOR))
            .map(|bytes| bytes / NV12_PLANE_RATIO_DENOMINATOR)
            .ok_or("Wayland shared-memory pool size overflowed")?;
        let pool_size = frame_size
            .checked_mul(BUFFER_POOL_SIZE)
            .ok_or("Wayland shared-memory pool size overflowed")?;
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("cannot connect to Wayland compositor: {error}"))?;
        let (globals, event_queue) = registry_queue_init::<PresenterState>(&connection)
            .map_err(|error| format!("cannot enumerate Wayland globals: {error}"))?;
        let qh = event_queue.handle();
        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|error| format!("Wayland compositor is unavailable: {error}"))?;
        let xdg_shell = XdgShell::bind(&globals, &qh)
            .map_err(|error| format!("Wayland XDG shell is unavailable: {error}"))?;
        let shm = Shm::bind(&globals, &qh)
            .map_err(|error| format!("Wayland shared memory is unavailable: {error}"))?;
        let surface = compositor.create_surface(&qh);
        let window = xdg_shell.create_window(surface, WindowDecorations::None, &qh);
        window.set_title("CarPlay");
        window.set_app_id("org.prelude.projection");
        window.set_min_size(Some((width_u32, height_u32)));
        window.set_max_size(Some((width_u32, height_u32)));
        window.set_fullscreen(None);
        window.commit();
        let pool = SlotPool::new(pool_size, &shm)
            .map_err(|error| format!("cannot allocate Wayland shared memory: {error}"))?;
        let mut presenter = Self {
            connection,
            event_queue,
            state: PresenterState {
                registry_state: RegistryState::new(&globals),
                output_state: OutputState::new(&globals, &qh),
                shm,
                pool,
                window,
                width: width_u32,
                height: height_u32,
                configured: false,
                configure_error: None,
                closed: false,
                buffers: Vec::with_capacity(BUFFER_POOL_SIZE),
            },
        };
        presenter
            .event_queue
            .roundtrip(&mut presenter.state)
            .map_err(|error| format!("Wayland initial configure failed: {error}"))?;
        if let Some(error) = presenter.state.configure_error.take() {
            return Err(error);
        }
        if !presenter.state.configured {
            return Err("Wayland compositor did not configure the CarPlay surface".to_owned());
        }
        Ok(presenter)
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.closed
    }

    pub(crate) fn present_nv12(
        &mut self,
        y_plane: &[u8],
        uv_plane: &[u8],
        y_pitch: usize,
        uv_pitch: usize,
    ) -> Result<(), String> {
        self.ensure_buffer_available()?;
        let width = usize::try_from(self.state.width).map_err(|_| "invalid Wayland width")?;
        let height = usize::try_from(self.state.height).map_err(|_| "invalid Wayland height")?;
        self.state.present(&self.event_queue.handle(), |canvas| {
            copy_nv12_to_shm(y_plane, uv_plane, y_pitch, uv_pitch, width, height, canvas)
        })?;
        self.connection
            .flush()
            .map_err(|error| format!("cannot flush Wayland frame: {error}"))?;
        Ok(())
    }

    pub(crate) fn present_packed_nv12(&mut self, frame: &[u8]) -> Result<(), String> {
        self.ensure_buffer_available()?;
        let width = usize::try_from(self.state.width).map_err(|_| "invalid Wayland width")?;
        let height = usize::try_from(self.state.height).map_err(|_| "invalid Wayland height")?;
        let expected = nv12_frame_size(width, height)?;
        if frame.len() != expected {
            return Err(format!(
                "packed NV12 frame is {} bytes, expected {expected}",
                frame.len()
            ));
        }
        self.state.present(&self.event_queue.handle(), |canvas| {
            canvas.copy_from_slice(frame);
            Ok(())
        })?;
        self.connection
            .flush()
            .map_err(|error| format!("cannot flush Wayland frame: {error}"))
    }

    pub(crate) fn clear(&mut self) -> Result<(), String> {
        self.ensure_buffer_available()?;
        let width = usize::try_from(self.state.width).map_err(|_| "invalid Wayland width")?;
        let height = usize::try_from(self.state.height).map_err(|_| "invalid Wayland height")?;
        self.state.present(&self.event_queue.handle(), |canvas| {
            fill_black_nv12(canvas, width, height)?;
            Ok(())
        })?;
        self.connection
            .flush()
            .map_err(|error| format!("cannot flush cleared Wayland frame: {error}"))
    }

    pub(crate) fn pump(&mut self) -> Result<(), String> {
        self.dispatch()?;
        self.connection
            .flush()
            .map_err(|error| format!("cannot flush Wayland connection: {error}"))
    }

    fn dispatch(&mut self) -> Result<(), String> {
        if let Some(guard) = self.event_queue.prepare_read() {
            let _ = guard.read();
        }
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| format!("Wayland event dispatch failed: {error}"))?;
        if let Some(error) = self.state.configure_error.take() {
            return Err(error);
        }
        if self.state.closed {
            return Err("Wayland compositor closed the CarPlay surface".to_owned());
        }
        Ok(())
    }

    fn ensure_buffer_available(&mut self) -> Result<(), String> {
        self.dispatch()?;
        if !self.state.has_free_buffer() {
            self.event_queue
                .roundtrip(&mut self.state)
                .map_err(|error| format!("Wayland fallback roundtrip failed: {error}"))?;
            if let Some(error) = self.state.configure_error.take() {
                return Err(error);
            }
            if self.state.closed {
                return Err("Wayland compositor closed the CarPlay surface".to_owned());
            }
        }
        Ok(())
    }
}

struct PresenterState {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    window: Window,
    width: u32,
    height: u32,
    configured: bool,
    configure_error: Option<String>,
    closed: bool,
    buffers: Vec<Nv12Buffer>,
}

#[derive(Debug)]
struct Nv12Buffer {
    buffer: Buffer,
    slot: smithay_client_toolkit::shm::slot::Slot,
}

impl PresenterState {
    fn has_free_buffer(&self) -> bool {
        self.buffers.len() < BUFFER_POOL_SIZE
            || self
                .buffers
                .iter()
                .any(|buf| !buf.slot.has_active_buffers())
    }

    fn present(
        &mut self,
        _qh: &QueueHandle<Self>,
        draw: impl FnOnce(&mut [u8]) -> Result<(), String>,
    ) -> Result<(), String> {
        let stride = i32::try_from(self.width).map_err(|_| "Wayland buffer stride overflowed")?;
        let width = i32::try_from(self.width).map_err(|_| "Wayland width is too large")?;
        let height = i32::try_from(self.height).map_err(|_| "Wayland height is too large")?;

        let mut target_index = None;
        for (i, buf) in self.buffers.iter().enumerate() {
            if !buf.slot.has_active_buffers() {
                target_index = Some(i);
                break;
            }
        }

        let index = match target_index {
            Some(i) => i,
            None if self.buffers.len() < BUFFER_POOL_SIZE => {
                let buffer = create_nv12_buffer(&mut self.pool, width, height, stride)?;
                self.buffers.push(buffer);
                self.buffers.len() - 1
            }
            None => {
                return Ok(());
            }
        };

        let frame = &self.buffers[index];
        let canvas = frame
            .slot
            .canvas(&mut self.pool)
            .ok_or("Wayland frame buffer canvas is unexpectedly unavailable")?;
        draw(canvas)?;
        self.window.wl_surface().damage_buffer(0, 0, width, height);
        frame
            .buffer
            .attach_to(self.window.wl_surface())
            .map_err(|error| format!("cannot attach Wayland frame buffer: {error}"))?;
        self.window.commit();
        Ok(())
    }
}

fn create_nv12_buffer(
    pool: &mut SlotPool,
    width: i32,
    height: i32,
    stride: i32,
) -> Result<Nv12Buffer, String> {
    let stride_usize = usize::try_from(stride).map_err(|_| "invalid Wayland NV12 stride")?;
    let height_usize = usize::try_from(height).map_err(|_| "invalid Wayland NV12 height")?;
    let frame_size = nv12_frame_size(stride_usize, height_usize)?;
    let slot = pool
        .new_slot(frame_size)
        .map_err(|error| format!("cannot allocate Wayland NV12 slot: {error}"))?;
    let buffer = pool
        .create_buffer_in(&slot, width, height, stride, wl_shm::Format::Nv12)
        .map_err(|error| format!("cannot allocate Wayland NV12 buffer: {error}"))?;
    Ok(Nv12Buffer { buffer, slot })
}

impl CompositorHandler for PresenterState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if new_factor != 1 {
            self.configure_error = Some(format!(
                "Wayland compositor requested unsupported scale factor {new_factor}"
            ));
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for PresenterState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl WindowHandler for PresenterState {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.closed = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &Window,
        configure: WindowConfigure,
        _: u32,
    ) {
        let requested_width = configure.new_size.0.map(|value| value.get());
        let requested_height = configure.new_size.1.map(|value| value.get());
        if requested_width.is_some_and(|value| value != self.width)
            || requested_height.is_some_and(|value| value != self.height)
        {
            self.configure_error = Some(format!(
                "Wayland compositor configured unexpected size {requested_width:?}x{requested_height:?}"
            ));
        }
        self.configured = true;
    }
}

impl ShmHandler for PresenterState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_compositor!(PresenterState);
delegate_output!(PresenterState);
delegate_shm!(PresenterState);
delegate_xdg_shell!(PresenterState);
delegate_xdg_window!(PresenterState);
delegate_registry!(PresenterState);

impl ProvidesRegistryState for PresenterState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

#[allow(clippy::too_many_arguments)]
fn copy_nv12_to_shm(
    y_plane: &[u8],
    uv_plane: &[u8],
    y_pitch: usize,
    uv_pitch: usize,
    width: usize,
    height: usize,
    output: &mut [u8],
) -> Result<(), String> {
    if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
        return Err("decoded NV12 geometry must have nonzero even dimensions".to_owned());
    }
    let y_required = y_pitch
        .checked_mul(height)
        .ok_or("decoded NV12 Y plane size overflowed")?;
    let uv_required = uv_pitch
        .checked_mul(height / 2)
        .ok_or("decoded NV12 UV plane size overflowed")?;
    let output_required = nv12_frame_size(width, height)?;
    if y_pitch < width
        || uv_pitch < width
        || y_plane.len() < y_required
        || uv_plane.len() < uv_required
        || output.len() < output_required
    {
        return Err(
            "decoded NV12 frame or Wayland output has an invalid size or stride".to_owned(),
        );
    }
    for row in 0..height {
        output[row * width..(row + 1) * width]
            .copy_from_slice(&y_plane[row * y_pitch..row * y_pitch + width]);
    }
    let output_uv = width * height;
    for row in 0..height / 2 {
        output[output_uv + row * width..output_uv + (row + 1) * width]
            .copy_from_slice(&uv_plane[row * uv_pitch..row * uv_pitch + width]);
    }
    Ok(())
}

fn fill_black_nv12(output: &mut [u8], width: usize, height: usize) -> Result<(), String> {
    let frame_size = nv12_frame_size(width, height)?;
    if output.len() < frame_size {
        return Err("Wayland NV12 buffer is shorter than one frame".to_owned());
    }
    let luma_bytes = width
        .checked_mul(height)
        .ok_or("Wayland NV12 luma size overflowed")?;
    output[..luma_bytes].fill(16);
    output[luma_bytes..frame_size].fill(128);
    Ok(())
}

fn nv12_frame_size(stride: usize, height: usize) -> Result<usize, String> {
    if stride == 0 || height == 0 || stride % 2 != 0 || height % 2 != 0 {
        return Err("Wayland NV12 geometry must have nonzero even dimensions".to_owned());
    }
    stride
        .checked_mul(height)
        .and_then(|luma| luma.checked_mul(NV12_PLANE_RATIO_NUMERATOR))
        .map(|bytes| bytes / NV12_PLANE_RATIO_DENOMINATOR)
        .ok_or_else(|| "Wayland NV12 frame size overflowed".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_is_copied_to_tightly_packed_shm_with_padded_input_strides() {
        let y = [16, 235, 0xaa, 0xaa, 81, 145, 0xbb, 0xbb];
        let uv = [128, 128, 0xcc, 0xcc];
        let mut output = [0xdd; 6];

        copy_nv12_to_shm(&y, &uv, 4, 4, 2, 2, &mut output).expect("valid NV12");

        assert_eq!(output, [16, 235, 81, 145, 128, 128]);
    }

    #[test]
    fn nv12_copy_rejects_invalid_geometry_and_storage() {
        let mut output = [0; 24];
        assert!(copy_nv12_to_shm(&[0; 8], &[0; 8], 4, 4, 3, 4, &mut output).is_err());
        assert!(copy_nv12_to_shm(&[0; 7], &[0; 8], 4, 4, 4, 2, &mut output).is_err());
        assert!(copy_nv12_to_shm(&[0; 8], &[0; 7], 4, 4, 4, 4, &mut output).is_err());
    }

    #[test]
    fn black_nv12_uses_limited_range_black_and_neutral_chroma() {
        let mut output = [0; 12];
        fill_black_nv12(&mut output, 4, 2).expect("black frame");
        assert_eq!(&output[..8], &[16; 8]);
        assert_eq!(&output[8..], &[128; 4]);
    }
}
