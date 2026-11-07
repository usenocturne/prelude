use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use platform_superbird::{
    AmbientLightControl, KnobInputButton, LinuxInputBackend, LinuxInputConfig, PlatformInputEvent,
    PlatformInputEventKind, RotaryDirection, TouchInputPhase,
};
use prelude_media::LinuxDisplayHandle;
use prelude_wireless::access_point::{Band, ChannelWidth};
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::{
    CarPlayDock, KnownPhone, PRESET_COUNT, PresetAction, PresetBindings, RuntimeSettings,
    WirelessPhoneDirectory,
    fonts::{self, FontSet},
};

mod equalizer_ui;
use equalizer_ui::EqualizerUi;

const DISPLAY_WIDTH: usize = 800;
const DISPLAY_HEIGHT: usize = 480;
const SETTINGS_POLL_INTERVAL: Duration = Duration::from_millis(20);
const INPUT_RETRY_INTERVAL: Duration = Duration::from_millis(500);
const PHONES_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const HEADER_HEIGHT: i32 = 84;
const VIEWPORT_TOP: i32 = HEADER_HEIGHT;
const TITLE_Y: i32 = 44;
const TITLE_X: i32 = 32;
const BACK_X: i32 = 34;
const BACK_TITLE_X: i32 = 64;
const GROUP_X: i32 = 24;
const GROUP_WIDTH: i32 = DISPLAY_WIDTH as i32 - GROUP_X * 2;
const GROUP_RADIUS: i32 = 18;
const ROW_INSET: i32 = 24;
const CONTENT_X: i32 = GROUP_X + ROW_INSET;
const CONTENT_RIGHT: i32 = GROUP_X + GROUP_WIDTH - ROW_INSET;
const ROW_HEIGHT: i32 = 84;
const SLIDER_ROW_HEIGHT: i32 = 140;
const CAPTION_HEIGHT: i32 = 44;
const FOOTER_HEIGHT: i32 = 44;
const SECTION_GAP: i32 = 12;
const CONTROL_GAP: i32 = 16;
const TITLE_SIZE: f32 = 38.0;
const LABEL_SIZE: f32 = 31.0;
const VALUE_SIZE: f32 = 28.0;
const DETAIL_SIZE: f32 = 20.0;
const CAPTION_SIZE: f32 = 19.0;
const SWITCH_WIDTH: i32 = 76;
const SWITCH_HEIGHT: i32 = 44;
const SLIDER_X: i32 = CONTENT_X;
const SLIDER_WIDTH: i32 = CONTENT_RIGHT - CONTENT_X;
const SLIDER_TRACK_OFFSET: i32 = 86;
const DRAG_THRESHOLD: i32 = 12;
const BANDS: [Band; 2] = [Band::Ghz2_4, Band::Ghz5];
const MIN_DPI: u32 = prelude_android_auto_protocol::MIN_VIDEO_DENSITY_DPI;
const MAX_DPI: u32 = prelude_android_auto_protocol::MAX_VIDEO_DENSITY_DPI;
const DPI_STEP: u32 = 10;
const GROUP: [u8; 3] = [28, 28, 30];
const FOCUS: [u8; 3] = [58, 58, 62];
const DIVIDER: [u8; 3] = [46, 46, 50];
const TRACK: [u8; 3] = [72, 72, 76];
const GREEN: [u8; 3] = [30, 215, 96];
const RED: [u8; 3] = [255, 69, 58];
const WHITE: [u8; 3] = [255, 255, 255];
const SECONDARY: [u8; 3] = [152, 152, 158];
const SUPPORT_QR_MODULES: i32 = 29;
const SUPPORT_QR: [u32; SUPPORT_QR_MODULES as usize] = [
    0x1fc0e47f, 0x10512141, 0x1758535d, 0x17566f5d, 0x174eef5d, 0x1042d941, 0x1fd5557f, 0x001b8200,
    0x105468ce, 0x03194236, 0x1a550c40, 0x133973e8, 0x1c6cc161, 0x1ab4c273, 0x1b51f1bc, 0x1a08d0b5,
    0x0ce6680c, 0x19a313f7, 0x1f53df49, 0x17367710, 0x114f0bf7, 0x00199118, 0x1fc3e75c, 0x10469713,
    0x1741a3f8, 0x1740bdae, 0x174f687e, 0x1049b2fd, 0x1fd03a3c,
];
const SUPPORT_CARD_X: i32 = 64;
const SUPPORT_CARD_SIZE: i32 = 280;
const SUPPORT_TEXT_X: i32 = SUPPORT_CARD_X + SUPPORT_CARD_SIZE + 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingsCommitted {
    pub settings: RuntimeSettings,
    pub restart_required: bool,
    pub radio_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostUiEvent {
    SettingsCommitted(SettingsCommitted),
    ConnectPhone(String),
    DisconnectPhone(String),
    ForgetPhone(String),
}

#[derive(Debug)]
pub struct HostUi {
    events: Receiver<HostUiEvent>,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    worker: Option<JoinHandle<()>>,
}

impl HostUi {
    pub fn start(
        display: LinuxDisplayHandle,
        state_dir: PathBuf,
        settings: RuntimeSettings,
        equalizer_control: prelude_media::EqualizerControl,
        presets: PresetBindings,
        ambient_light: Option<AmbientLightControl>,
        phones: Option<WirelessPhoneDirectory>,
    ) -> Result<Self, String> {
        let (events_tx, events) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let worker_stop = Arc::clone(&stop);
        let worker_wake = Arc::clone(&wake);
        let worker = thread::Builder::new()
            .name("prelude-host-ui".to_owned())
            .spawn(move || {
                let ui = SettingsUi::new(
                    display,
                    state_dir,
                    settings,
                    equalizer_control,
                    presets,
                    ambient_light,
                    phones,
                    events_tx,
                    worker_wake,
                );
                run_host_ui(ui, &worker_stop);
            })
            .map_err(|error| format!("starting the host settings UI failed: {error}"))?;
        Ok(Self {
            events,
            stop,
            wake,
            worker: Some(worker),
        })
    }

    pub fn take_events(&self) -> Vec<HostUiEvent> {
        self.events.try_iter().collect()
    }

    pub fn wake(&self) -> Arc<Notify> {
        Arc::clone(&self.wake)
    }
}

impl Drop for HostUi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

trait OverlaySurface {
    fn set_overlay(&self, frame: Vec<u8>) -> prelude_media::Result<()>;
    fn clear_overlay(&self) -> prelude_media::Result<()>;
    fn set_host_ui_active(&self, active: bool) -> prelude_media::Result<()>;
}

impl OverlaySurface for LinuxDisplayHandle {
    fn set_overlay(&self, frame: Vec<u8>) -> prelude_media::Result<()> {
        Self::set_overlay(self, frame)
    }

    fn clear_overlay(&self) -> prelude_media::Result<()> {
        Self::clear_overlay(self)
    }

    fn set_host_ui_active(&self, active: bool) -> prelude_media::Result<()> {
        Self::set_host_ui_active(self, active)
    }
}

fn run_host_ui(mut ui: SettingsUi<impl OverlaySurface>, stop: &AtomicBool) {
    let mut input = None;
    while !stop.load(Ordering::Acquire) {
        if input.is_none() {
            match LinuxInputBackend::discover(LinuxInputConfig::default()) {
                Ok(backend) => {
                    info!("host settings UI acquired Superbird input");
                    input = Some(backend);
                }
                Err(error) => {
                    debug!(%error, "host settings UI input is not ready");
                    wait_for_stop(stop, INPUT_RETRY_INTERVAL);
                    continue;
                }
            }
        }

        let Some(backend) = input.as_mut() else {
            continue;
        };
        match backend.poll() {
            Ok(batch) => {
                for event in batch.events {
                    ui.handle(event);
                }
                if batch.terminal.is_some() {
                    ui.close_without_commit();
                    input = None;
                }
            }
            Err(error) => {
                warn!(%error, "host settings UI input stopped; retrying");
                input = None;
                wait_for_stop(stop, INPUT_RETRY_INTERVAL);
            }
        }
        ui.tick();
    }
    ui.close_without_commit();
}

fn wait_for_stop(stop: &AtomicBool, duration: Duration) {
    let start = std::time::Instant::now();
    while !stop.load(Ordering::Acquire) && start.elapsed() < duration {
        thread::sleep(SETTINGS_POLL_INTERVAL.min(duration.saturating_sub(start.elapsed())));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Settings,
    Phones,
    Phone,
    Density,
    Presets,
    Wifi,
    Equalizer,
    Support,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Phones,
    Wifi,
    AutoBrightness,
    AutoNightMode,
    Density,
    CarPlayDock,
    PhoneAudio,
    Equalizer,
    Presets,
    Preset(usize),
    PresetsReset,
    Support,
    Version,
    Phone(usize),
    Connect,
    Forget,
    DensitySlider,
    DensityReset,
    WifiBand,
    WifiChannel,
    WifiWidth,
}

impl Item {
    const fn adjustable(self) -> bool {
        matches!(
            self,
            Self::DensitySlider
                | Self::CarPlayDock
                | Self::Preset(_)
                | Self::WifiBand
                | Self::WifiChannel
                | Self::WifiWidth
        )
    }

    const fn height(self) -> i32 {
        match self {
            Self::DensitySlider => SLIDER_ROW_HEIGHT,
            _ => ROW_HEIGHT,
        }
    }
}

struct Section {
    title: Option<String>,
    items: Vec<Item>,
    footer: Option<&'static str>,
}

impl Section {
    fn titled(title: impl Into<String>, items: Vec<Item>) -> Self {
        Self {
            title: Some(title.into()),
            items,
            footer: None,
        }
    }

    fn plain(items: Vec<Item>) -> Self {
        Self {
            title: None,
            items,
            footer: None,
        }
    }

    fn footer(self, footer: &'static str) -> Self {
        Self {
            footer: Some(footer),
            ..self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    item: Item,
    top: i32,
    stop: i32,
    first: bool,
    last: bool,
}

impl Row {
    const fn bottom(&self) -> i32 {
        self.top + self.item.height()
    }
}

#[derive(Debug, Default)]
struct Layout {
    rows: Vec<Row>,
    captions: Vec<(String, i32)>,
    footers: Vec<(&'static str, i32)>,
    groups: Vec<(i32, i32)>,
    bottom: i32,
}

impl Layout {
    fn new(sections: Vec<Section>) -> Self {
        let mut layout = Self::default();
        let mut y = HEADER_HEIGHT;
        for section in sections
            .into_iter()
            .filter(|section| !section.items.is_empty())
        {
            let stop = y;
            if let Some(title) = section.title {
                layout.captions.push((title, y + CAPTION_HEIGHT / 2 + 2));
                y += CAPTION_HEIGHT;
            }
            let top = y;
            let count = section.items.len();
            for (index, item) in section.items.into_iter().enumerate() {
                layout.rows.push(Row {
                    item,
                    top: y,
                    stop: if index == 0 { stop } else { y },
                    first: index == 0,
                    last: index + 1 == count,
                });
                y += item.height();
            }
            layout.groups.push((top, y));
            if let Some(footer) = section.footer {
                layout.footers.push((footer, y + FOOTER_HEIGHT / 2));
                y += FOOTER_HEIGHT;
            }
            y += SECTION_GAP;
        }
        layout.bottom = y;
        layout
    }

    fn maximum_scroll(&self) -> i32 {
        (self.bottom - DISPLAY_HEIGHT as i32).max(0)
    }

    fn row_at(&self, y: i32) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| (row.top..row.bottom()).contains(&y))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Touch {
    x: i32,
    y: i32,
    scroll: i32,
    row: Option<usize>,
    header: bool,
    scrolling: bool,
    sliding: bool,
}

enum Trailing {
    None,
    Switch(bool),
    Value(String, [u8; 3]),
    Link(String, [u8; 3]),
}

struct RowView {
    label: String,
    label_color: [u8; 3],
    detail: Option<String>,
    trailing: Trailing,
}

impl RowView {
    fn new(label: impl Into<String>, trailing: Trailing) -> Self {
        Self {
            label: label.into(),
            label_color: WHITE,
            detail: None,
            trailing,
        }
    }

    fn detail(self, detail: impl Into<String>) -> Self {
        Self {
            detail: Some(detail.into()),
            ..self
        }
    }

    fn color(self, label_color: [u8; 3]) -> Self {
        Self {
            label_color,
            ..self
        }
    }
}

struct SettingsUi<D> {
    display: D,
    equalizer_control: prelude_media::EqualizerControl,
    equalizer_ui: EqualizerUi,
    state_dir: PathBuf,
    settings: RuntimeSettings,
    staged_dpi: u32,
    staged_dpi_customized: bool,
    staged_phone_audio: bool,
    staged_dock: CarPlayDock,
    presets: PresetBindings,
    staged_presets: [PresetAction; PRESET_COUNT],
    staged_radio: Option<(u8, ChannelWidth)>,
    page: Page,
    focus: usize,
    settings_focus: usize,
    phone_focus: usize,
    editing: bool,
    touch: Option<Touch>,
    scroll: i32,
    menu_open: bool,
    error_message: Option<&'static str>,
    ambient_light: Option<AmbientLightControl>,
    directory: Option<WirelessPhoneDirectory>,
    wireless: bool,
    version: String,
    phones: Vec<KnownPhone>,
    active_phone: Option<String>,
    opened_phone: Option<String>,
    confirming_forget: bool,
    pending_connect: Option<String>,
    phones_refreshed: Instant,
    events: Sender<HostUiEvent>,
    wake: Arc<Notify>,
    fonts: &'static FontSet,
}

impl<D: OverlaySurface> SettingsUi<D> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        display: D,
        state_dir: PathBuf,
        settings: RuntimeSettings,
        equalizer_control: prelude_media::EqualizerControl,
        presets: PresetBindings,
        ambient_light: Option<AmbientLightControl>,
        directory: Option<WirelessPhoneDirectory>,
        events: Sender<HostUiEvent>,
        wake: Arc<Notify>,
    ) -> Self {
        if let Some(control) = ambient_light.as_ref() {
            control.set_automatic_brightness(settings.auto_brightness);
            control.set_automatic_night_mode(settings.auto_night_mode);
        }
        Self {
            display,
            equalizer_control,
            equalizer_ui: EqualizerUi::new(settings.equalizer),
            state_dir,
            settings,
            staged_dpi: settings.reported_dpi,
            staged_dpi_customized: settings.dpi_customized,
            staged_phone_audio: settings.phone_audio,
            staged_dock: settings.carplay_dock,
            presets,
            staged_presets: settings.presets,
            staged_radio: settings.wifi_radio(),
            page: Page::Settings,
            focus: 0,
            settings_focus: 0,
            phone_focus: 0,
            editing: false,
            touch: None,
            scroll: 0,
            menu_open: false,
            error_message: None,
            ambient_light,
            wireless: directory.is_some(),
            directory,
            version: system_version(),
            phones: Vec::new(),
            active_phone: None,
            opened_phone: None,
            confirming_forget: false,
            pending_connect: None,
            phones_refreshed: Instant::now(),
            events,
            wake,
            fonts: fonts::shared(),
        }
    }

    fn handle(&mut self, event: PlatformInputEvent) {
        match event.kind {
            PlatformInputEventKind::SettingsButton => {
                if self.menu_open {
                    self.close();
                } else {
                    self.open();
                }
            }
            PlatformInputEventKind::KnobButton { button, pressed }
                if self.menu_open && !pressed =>
            {
                if self.page == Page::Equalizer {
                    self.handle_equalizer_button(button);
                } else {
                    self.handle_button(button);
                }
            }
            PlatformInputEventKind::Rotary { direction, steps } if self.menu_open => {
                if self.page == Page::Equalizer {
                    self.equalizer_ui.rotary(direction, steps);
                    self.apply_equalizer();
                } else {
                    self.handle_rotary(direction, steps);
                }
            }
            PlatformInputEventKind::Touch {
                contact_id: 0,
                phase,
                x,
                y,
            } if self.menu_open => {
                if self.page == Page::Equalizer {
                    self.handle_equalizer_touch(phase, x, y);
                } else {
                    self.handle_touch(phase, i32::from(x), i32::from(y));
                }
            }
            PlatformInputEventKind::Reset { .. } if self.menu_open => {
                self.equalizer_ui.cancel_interaction();
                self.save_equalizer();
                self.touch = None;
                self.editing = false;
                self.redraw();
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        if self.menu_open && self.phones_refreshed.elapsed() >= PHONES_REFRESH_INTERVAL {
            let phones = self.phones.clone();
            let active = self.active_phone.clone();
            self.refresh_phones();
            if self.page == Page::Equalizer || phones != self.phones || active != self.active_phone
            {
                self.redraw();
            }
        }
    }

    fn refresh_phones(&mut self) {
        self.phones_refreshed = Instant::now();
        let Some(directory) = self.directory.as_ref() else {
            return;
        };
        self.phones = directory.phones();
        self.active_phone = directory.active();
        if self.pending_connect.is_some() && self.pending_connect == self.active_phone {
            self.pending_connect = None;
        }
        self.phone_focus = self.phone_focus.min(self.phones.len().saturating_sub(1));
        match self.page {
            Page::Phone if self.opened().is_none() => self.show_phones(self.phone_focus),
            Page::Phones => self.focus = self.focus.min(self.phones.len().saturating_sub(1)),
            _ => {}
        }
        self.scroll = self.scroll.min(self.layout().maximum_scroll());
    }

    fn opened(&self) -> Option<&KnownPhone> {
        let address = self.opened_phone.as_deref()?;
        self.phones.iter().find(|phone| phone.address == address)
    }

    fn wifi_available(&self) -> bool {
        self.wireless && self.staged_radio.is_some()
    }

    fn has_staged_changes(&self) -> bool {
        self.staged_dpi != self.settings.reported_dpi
            || self.staged_dpi_customized != self.settings.dpi_customized
            || self.staged_phone_audio != self.settings.phone_audio
            || self.staged_dock != self.settings.carplay_dock
            || self.staged_presets != self.settings.presets
            || self.staged_radio != self.settings.wifi_radio()
    }

    fn sections(&self) -> Vec<Section> {
        match self.page {
            Page::Settings => vec![
                Section::plain(vec![Item::Phones]),
                Section::titled(
                    "Display",
                    vec![
                        Item::AutoBrightness,
                        Item::AutoNightMode,
                        Item::Density,
                        Item::CarPlayDock,
                    ],
                ),
                Section::titled("Audio", vec![Item::PhoneAudio, Item::Equalizer]),
                Section::titled("Controls", vec![Item::Presets]),
                Section::titled("Advanced", vec![Item::Wifi, Item::Support, Item::Version]),
            ],
            Page::Phones => vec![Section::plain(
                (0..self.phones.len()).map(Item::Phone).collect(),
            )],
            Page::Phone => self
                .opened()
                .map(|phone| {
                    vec![Section {
                        title: self.phone_status(phone).map(str::to_owned),
                        ..Section::plain(vec![Item::Connect, Item::Forget])
                    }]
                })
                .unwrap_or_default(),
            Page::Density => vec![
                Section::plain(vec![Item::DensitySlider, Item::DensityReset])
                    .footer("CarPlay and Android Auto restart after settings close"),
            ],
            Page::Presets => vec![
                Section::plain((0..PRESET_COUNT).map(Item::Preset).collect()),
                Section::plain(vec![Item::PresetsReset]),
            ],
            Page::Wifi if self.wifi_available() => vec![
                Section::plain(vec![Item::WifiBand, Item::WifiChannel, Item::WifiWidth])
                    .footer("Wi-Fi restarts after settings close"),
            ],
            Page::Wifi | Page::Equalizer | Page::Support => Vec::new(),
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(self.sections())
    }

    fn focused_item(&self) -> Option<Item> {
        self.layout().rows.get(self.focus).map(|row| row.item)
    }

    fn focus_row(&mut self, row: usize) {
        if row != self.focus {
            self.confirming_forget = false;
            self.editing = false;
        }
        self.focus = row;
    }

    fn scroll_to_focus(&mut self) {
        let layout = self.layout();
        let Some(row) = layout.rows.get(self.focus) else {
            self.scroll = 0;
            return;
        };
        let hidden_below = row.bottom() + SECTION_GAP - DISPLAY_HEIGHT as i32;
        self.scroll = self
            .scroll
            .max(hidden_below)
            .min(row.stop - HEADER_HEIGHT)
            .clamp(0, layout.maximum_scroll());
    }

    fn content_y(&self, y: i32) -> Option<i32> {
        (y >= VIEWPORT_TOP).then_some(y + self.scroll)
    }

    fn open(&mut self) {
        if let Err(error) = self.display.set_host_ui_active(true) {
            warn!(%error, "could not mark the host settings UI active");
            return;
        }
        self.menu_open = true;
        self.page = Page::Settings;
        self.focus = 0;
        self.settings_focus = 0;
        self.scroll = 0;
        self.staged_dpi = self.settings.reported_dpi;
        self.staged_dpi_customized = self.settings.dpi_customized;
        self.staged_phone_audio = self.settings.phone_audio;
        self.staged_dock = self.settings.carplay_dock;
        self.staged_presets = self.settings.presets;
        self.staged_radio = self.settings.wifi_radio();
        self.editing = false;
        self.touch = None;
        self.error_message = None;
        self.refresh_phones();
        self.wake.notify_one();
        if let Err(error) = self.display.set_overlay(self.render()) {
            warn!(%error, "could not render the host settings UI");
            let _ = self.display.set_host_ui_active(false);
            self.menu_open = false;
            self.wake.notify_one();
        }
    }

    fn close(&mut self) {
        let mut next = self.settings;
        next.equalizer = self.equalizer_ui.settings;
        next.reported_dpi = self.staged_dpi;
        next.dpi_customized = self.staged_dpi_customized;
        next.phone_audio = self.staged_phone_audio;
        next.carplay_dock = self.staged_dock;
        next.presets = self.staged_presets;
        if let Some((channel, width)) = self.staged_radio {
            next.wifi_channel = Some(channel);
            next.wifi_channel_width_mhz = Some(width.mhz());
        }
        if let Err(error) = next.persist(&self.state_dir) {
            warn!(%error, "could not persist host settings");
            self.error_message = Some("Unable to save settings");
            self.redraw();
            return;
        }
        let restart_required = next.reported_dpi != self.settings.reported_dpi
            || next.dpi_customized != self.settings.dpi_customized
            || next.phone_audio != self.settings.phone_audio
            || next.carplay_dock != self.settings.carplay_dock;
        let radio_changed = next.wifi_radio() != self.settings.wifi_radio();
        self.presets.set(next.presets);
        self.settings = next;
        self.menu_open = false;
        self.leave_subpage();
        if let Err(error) = self.display.clear_overlay() {
            warn!(%error, "could not clear the host settings UI");
        }
        if let Err(error) = self.display.set_host_ui_active(false) {
            warn!(%error, "could not mark the host settings UI inactive");
        }
        if restart_required || radio_changed {
            self.send(HostUiEvent::SettingsCommitted(SettingsCommitted {
                settings: next,
                restart_required,
                radio_changed,
            }));
        }
        self.wake.notify_one();
    }

    fn close_without_commit(&mut self) {
        if self.menu_open {
            let _ = self.save_equalizer();
            if let Err(error) = self.display.clear_overlay() {
                warn!(%error, "could not clear the host settings UI during shutdown");
            }
            if let Err(error) = self.display.set_host_ui_active(false) {
                warn!(%error, "could not release the host settings UI during shutdown");
            }
            self.menu_open = false;
            self.leave_subpage();
            self.wake.notify_one();
        }
    }

    fn send(&self, event: HostUiEvent) {
        if let Err(error) = self.events.send(event) {
            warn!(%error, "could not notify the daemon about a settings change");
        }
        self.wake.notify_one();
    }

    fn handle_button(&mut self, button: KnobInputButton) {
        match button {
            KnobInputButton::Back if self.editing => {
                self.editing = false;
                self.redraw();
            }
            KnobInputButton::Back if self.page == Page::Settings => self.close(),
            KnobInputButton::Back => self.go_back(),
            KnobInputButton::Select => match self.focused_item() {
                Some(item) if item.adjustable() => {
                    self.editing = !self.editing;
                    self.redraw();
                }
                Some(item) => self.activate(item),
                None => {}
            },
        }
    }

    fn handle_rotary(&mut self, direction: RotaryDirection, steps: u8) {
        if self.editing {
            if let Some(item) = self.focused_item() {
                let steps = isize::from(steps);
                let delta = match direction {
                    RotaryDirection::Clockwise => steps,
                    RotaryDirection::CounterClockwise => -steps,
                };
                self.adjust(item, delta, false);
            }
            return;
        }
        let Some(last) = self.layout().rows.len().checked_sub(1) else {
            return;
        };
        self.focus_row(selection_after_rotary(self.focus, direction, steps, last));
        self.scroll_to_focus();
        self.redraw();
    }

    fn handle_touch(&mut self, phase: TouchInputPhase, x: i32, y: i32) {
        match phase {
            TouchInputPhase::Down => {
                let row = self.content_y(y).and_then(|y| self.layout().row_at(y));
                self.touch = Some(Touch {
                    x,
                    y,
                    scroll: self.scroll,
                    row,
                    header: y < HEADER_HEIGHT,
                    scrolling: false,
                    sliding: false,
                });
                if let Some(row) = row {
                    self.focus_row(row);
                }
                self.editing = false;
                self.redraw();
            }
            TouchInputPhase::Move => self.touch_moved(x, y),
            TouchInputPhase::Up => self.touch_released(x, y),
        }
    }

    fn touch_moved(&mut self, x: i32, y: i32) {
        let Some(mut touch) = self.touch else {
            return;
        };
        let (dx, dy) = (x - touch.x, y - touch.y);
        let maximum = self.layout().maximum_scroll();
        if !touch.scrolling && !touch.sliding {
            let on_slider = touch.row.is_some() && self.focused_item() == Some(Item::DensitySlider);
            if on_slider && dx.abs() >= DRAG_THRESHOLD && dx.abs() > dy.abs() {
                touch.sliding = true;
            } else if dy.abs() >= DRAG_THRESHOLD && dy.abs() >= dx.abs() && maximum > 0 {
                touch.scrolling = true;
            }
        }
        self.touch = Some(touch);
        if touch.sliding {
            self.stage_dpi(dpi_from_x(x));
        } else if touch.scrolling {
            self.scroll = (touch.scroll - dy).clamp(0, maximum);
            self.redraw();
        }
    }

    fn touch_released(&mut self, x: i32, y: i32) {
        let Some(touch) = self.touch.take() else {
            return;
        };
        if touch.scrolling || touch.sliding {
            self.redraw();
            return;
        }
        if touch.header {
            if y < HEADER_HEIGHT && self.page != Page::Settings {
                self.go_back();
            }
            return;
        }
        let layout = self.layout();
        let Some(row) = touch.row.and_then(|index| layout.rows.get(index)) else {
            return;
        };
        let Some(content_y) = self.content_y(y).filter(|&y| layout.row_at(y) == touch.row) else {
            return;
        };
        match row.item {
            Item::DensitySlider if content_y >= row.top + SLIDER_TRACK_OFFSET - 28 => {
                self.stage_dpi(dpi_from_x(x));
            }
            Item::DensitySlider => {}
            item if item.adjustable() => self.adjust(item, 1, true),
            item => self.activate(item),
        }
    }

    fn activate(&mut self, item: Item) {
        match item {
            Item::Phones => {
                self.refresh_phones();
                self.enter(Page::Phones);
            }
            Item::Wifi => self.enter(Page::Wifi),
            Item::AutoBrightness => self.toggle_auto_brightness(),
            Item::AutoNightMode => self.toggle_auto_night_mode(),
            Item::Density => self.enter(Page::Density),
            Item::PhoneAudio => self.toggle_phone_audio(),
            Item::Equalizer => self.enter(Page::Equalizer),
            Item::Presets => self.enter(Page::Presets),
            Item::PresetsReset => self.reset_presets(),
            Item::Support => self.enter(Page::Support),
            Item::Version => {}
            Item::Phone(index) => self.open_phone(index),
            Item::Connect => self.connect_opened_phone(),
            Item::Forget => self.forget_opened_phone(),
            Item::DensityReset => self.restore_original_dpi(),
            Item::DensitySlider
            | Item::CarPlayDock
            | Item::Preset(_)
            | Item::WifiBand
            | Item::WifiChannel
            | Item::WifiWidth => self.adjust(item, 1, true),
        }
    }

    fn adjust(&mut self, item: Item, delta: isize, wrap: bool) {
        match item {
            Item::DensitySlider => {
                let dpi = i64::from(self.staged_dpi) + delta as i64 * i64::from(DPI_STEP);
                let dpi = dpi.clamp(i64::from(MIN_DPI), i64::from(MAX_DPI));
                self.stage_dpi(u32::try_from(dpi).unwrap_or(MIN_DPI));
            }
            Item::CarPlayDock => {
                self.staged_dock = step_option(&CarPlayDock::ALL, self.staged_dock, delta, wrap);
                self.error_message = None;
                self.redraw();
            }
            Item::Preset(index) => {
                if let Some(action) = self.staged_presets.get_mut(index) {
                    *action = step_option(&PresetAction::ALL, *action, delta, wrap);
                    self.error_message = None;
                    self.redraw();
                }
            }
            Item::WifiBand | Item::WifiChannel | Item::WifiWidth => {
                if let Some(radio) = self.staged_radio {
                    self.staged_radio = Some(step_radio(radio, item, delta, wrap));
                    self.error_message = None;
                    self.redraw();
                }
            }
            _ => {}
        }
    }

    fn stage_dpi(&mut self, dpi: u32) {
        self.staged_dpi = dpi;
        self.staged_dpi_customized = true;
        self.error_message = None;
        self.redraw();
    }

    fn enter(&mut self, page: Page) {
        if self.page == Page::Settings {
            self.settings_focus = self.focus;
        }
        self.equalizer_ui.cancel_interaction();
        if page == Page::Equalizer {
            self.equalizer_ui.focus_toggle();
        }
        self.page = page;
        self.focus = 0;
        self.scroll = 0;
        self.editing = false;
        self.confirming_forget = false;
        self.redraw();
    }

    fn show_phones(&mut self, focus: usize) {
        self.page = Page::Phones;
        self.opened_phone = None;
        self.confirming_forget = false;
        self.editing = false;
        self.focus = focus.min(self.phones.len().saturating_sub(1));
        self.scroll = 0;
        self.scroll_to_focus();
        self.redraw();
    }

    fn open_phone(&mut self, index: usize) {
        let Some(phone) = self.phones.get(index) else {
            return;
        };
        self.opened_phone = Some(phone.address.clone());
        self.phone_focus = index;
        self.enter(Page::Phone);
    }

    fn apply_equalizer(&mut self) {
        if let Err(error) = self.equalizer_control.set(self.equalizer_ui.settings) {
            warn!(%error, "could not apply equalizer");
            self.error_message = Some("Unable to apply equalizer");
        } else {
            self.error_message = None;
        }
        self.redraw();
    }

    fn save_equalizer(&mut self) -> bool {
        if !self.equalizer_ui.dirty {
            return true;
        }
        match self.equalizer_ui.persist(&self.state_dir, self.settings) {
            Ok(next) => self.settings = next,
            Err(error) => {
                warn!(%error, "could not save equalizer");
                self.error_message = Some("Unable to save settings");
                self.redraw();
                return false;
            }
        }
        self.error_message = None;
        true
    }

    fn handle_equalizer_button(&mut self, button: KnobInputButton) {
        let leave = self.equalizer_ui.button(button);
        self.apply_equalizer();
        if self.save_equalizer() && leave {
            self.go_back();
        }
    }

    fn handle_equalizer_touch(&mut self, phase: TouchInputPhase, x: u16, y: u16) {
        let leave = self.equalizer_ui.touch(phase, x, y);
        self.apply_equalizer();
        if phase == TouchInputPhase::Up && self.save_equalizer() && leave {
            self.go_back();
        }
    }

    fn leave_subpage(&mut self) {
        self.equalizer_ui.cancel_interaction();
        if self.page != Page::Settings {
            self.page = Page::Settings;
            self.focus = self.settings_focus;
            self.scroll = 0;
        }
        self.opened_phone = None;
        self.confirming_forget = false;
        self.editing = false;
        self.touch = None;
        self.scroll_to_focus();
    }

    fn go_back(&mut self) {
        match self.page {
            Page::Settings => {}
            Page::Phone => self.show_phones(self.phone_focus),
            _ => {
                self.leave_subpage();
                self.redraw();
            }
        }
    }

    fn connect_opened_phone(&mut self) {
        let Some(phone) = self.opened().cloned() else {
            return;
        };
        self.confirming_forget = false;
        if self.is_active(&phone) {
            info!(peer = phone.address, "disconnecting a phone from settings");
            self.active_phone = None;
            self.send(HostUiEvent::DisconnectPhone(phone.address));
        } else {
            info!(
                peer = phone.address,
                "connecting to a phone chosen in settings"
            );
            self.pending_connect = Some(phone.address.clone());
            self.send(HostUiEvent::ConnectPhone(phone.address));
        }
        self.redraw();
    }

    fn forget_opened_phone(&mut self) {
        let Some(phone) = self.opened().cloned() else {
            return;
        };
        if !self.confirming_forget {
            self.confirming_forget = true;
            self.redraw();
            return;
        }
        info!(peer = phone.address, "forgetting a phone from settings");
        if self.pending_connect.as_deref() == Some(phone.address.as_str()) {
            self.pending_connect = None;
        }
        self.phones.retain(|known| known.address != phone.address);
        self.send(HostUiEvent::ForgetPhone(phone.address));
        self.show_phones(self.phone_focus);
    }

    fn restore_original_dpi(&mut self) {
        self.staged_dpi = self.settings.original_dpi;
        self.staged_dpi_customized = false;
        self.editing = false;
        self.error_message = None;
        self.redraw();
    }

    fn reset_presets(&mut self) {
        self.staged_presets = PresetAction::DEFAULTS;
        self.editing = false;
        self.error_message = None;
        self.redraw();
    }

    fn toggle_auto_brightness(&mut self) {
        let mut next = self.settings;
        next.auto_brightness = !next.auto_brightness;
        self.apply_immediate_setting(next, true);
    }

    fn toggle_auto_night_mode(&mut self) {
        let mut next = self.settings;
        next.auto_night_mode = !next.auto_night_mode;
        self.apply_immediate_setting(next, false);
    }

    fn toggle_phone_audio(&mut self) {
        self.staged_phone_audio = !self.staged_phone_audio;
        self.error_message = None;
        self.redraw();
    }

    fn apply_immediate_setting(&mut self, next: RuntimeSettings, brightness: bool) {
        if let Err(error) = next.persist(&self.state_dir) {
            warn!(%error, "could not persist host settings");
            self.error_message = Some("Unable to save settings");
            self.redraw();
            return;
        }
        if let Some(control) = self.ambient_light.as_ref() {
            if brightness {
                control.set_automatic_brightness(next.auto_brightness);
            } else {
                control.set_automatic_night_mode(next.auto_night_mode);
            }
        }
        self.settings = next;
        self.error_message = None;
        self.redraw();
    }

    fn is_active(&self, phone: &KnownPhone) -> bool {
        self.active_phone.as_deref() == Some(phone.address.as_str())
    }

    fn is_pending(&self, phone: &KnownPhone) -> bool {
        self.pending_connect.as_deref() == Some(phone.address.as_str())
    }

    fn phone_status(&self, phone: &KnownPhone) -> Option<&'static str> {
        if self.is_active(phone) {
            Some("Connected")
        } else if self.is_pending(phone) {
            Some("Connecting…")
        } else {
            None
        }
    }

    fn redraw(&self) {
        if self.menu_open
            && let Err(error) = self.display.set_overlay(self.render())
        {
            warn!(%error, "could not update the host settings UI");
        }
    }
}

impl<D: OverlaySurface> SettingsUi<D> {
    fn render(&self) -> Vec<u8> {
        let mut canvas = Canvas::new();
        self.render_into(&mut canvas);
        canvas.to_nv12()
    }

    fn render_into(&self, canvas: &mut Canvas) {
        let layout = self.layout();
        canvas.scroll(self.scroll, HEADER_HEIGHT);
        match self.page {
            Page::Equalizer => self.equalizer_ui.render(
                canvas,
                self.fonts,
                self.staged_phone_audio,
                self.equalizer_control.availability(),
                self.error_message,
            ),
            Page::Support => self.render_support(canvas),
            _ if layout.rows.is_empty() => self.render_empty(canvas),
            _ => self.render_layout(canvas, &layout),
        }
        canvas.scroll(0, 0);
        if self.scroll > 0 {
            canvas.rounded_rect(
                0,
                HEADER_HEIGHT - 1,
                DISPLAY_WIDTH as i32,
                1,
                0,
                DIVIDER,
                255,
            );
        }
        self.render_scrollbar(canvas, &layout);
        self.render_header(canvas);
    }

    fn title(&self) -> &str {
        match self.page {
            Page::Settings => "Settings",
            Page::Phones => "Phones",
            Page::Phone => self.opened().map_or("Phone", |phone| phone.name.as_str()),
            Page::Density => "Screen density",
            Page::Presets => "Preset buttons",
            Page::Wifi => "Wi-Fi",
            Page::Equalizer => "Equalizer",
            Page::Support => "Support Prelude",
        }
    }

    fn render_header(&self, canvas: &mut Canvas) {
        let note = match self.page {
            Page::Equalizer => None,
            _ if self.error_message.is_some() => self.error_message.map(|message| (message, RED)),
            Page::Settings if self.has_staged_changes() => {
                Some(("Applies when settings close", SECONDARY))
            }
            _ => None,
        };
        if self.page == Page::Settings {
            canvas.text(
                self.fonts,
                self.title(),
                TITLE_X,
                TITLE_Y,
                TITLE_SIZE,
                WHITE,
                true,
            );
        } else {
            let right = match note {
                _ if self.page == Page::Equalizer => equalizer_ui::HEADER_LEFT,
                Some((note, _)) => {
                    CONTENT_RIGHT - self.fonts.text_width(note, CAPTION_SIZE, false) - CONTROL_GAP
                }
                None => CONTENT_RIGHT,
            };
            canvas.chevron(BACK_X, TITLE_Y, 12, false, WHITE, 4.0);
            let title = self
                .fonts
                .fit(self.title(), TITLE_SIZE, true, right - BACK_TITLE_X);
            canvas.text(
                self.fonts,
                &title,
                BACK_TITLE_X,
                TITLE_Y,
                TITLE_SIZE,
                WHITE,
                true,
            );
        }
        if self.page == Page::Equalizer {
            self.equalizer_ui.render_header(canvas, self.fonts);
        } else if let Some((note, color)) = note {
            canvas.text_right(
                self.fonts,
                note,
                CONTENT_RIGHT,
                TITLE_Y,
                CAPTION_SIZE,
                color,
                false,
            );
        }
    }

    fn render_scrollbar(&self, canvas: &mut Canvas, layout: &Layout) {
        let maximum = layout.maximum_scroll();
        if maximum == 0 {
            return;
        }
        let top = HEADER_HEIGHT + 8;
        let track = DISPLAY_HEIGHT as i32 - 8 - top;
        let visible = DISPLAY_HEIGHT as i32 - HEADER_HEIGHT;
        let thumb = (track * visible / (visible + maximum)).max(32);
        let y = top + (track - thumb) * self.scroll / maximum;
        canvas.rounded_rect(DISPLAY_WIDTH as i32 - 10, y, 4, thumb, 2, WHITE, 90);
    }

    fn render_empty(&self, canvas: &mut Canvas) {
        let (headline, caption) = match self.page {
            Page::Phones if !self.wireless => (
                "Wireless is off",
                "Enable wireless projection to manage phones",
            ),
            Page::Phones => (
                "No saved phones",
                "Phones you connect wirelessly show up here",
            ),
            Page::Wifi => (
                "Wireless is off",
                "Enable wireless projection to change Wi-Fi settings",
            ),
            _ => return,
        };
        let center_x = DISPLAY_WIDTH as i32 / 2;
        let center_y = (HEADER_HEIGHT + DISPLAY_HEIGHT as i32) / 2 - 12;
        canvas.text_center(
            self.fonts,
            headline,
            center_x,
            center_y - 18,
            LABEL_SIZE,
            WHITE,
            true,
        );
        canvas.text_center(
            self.fonts,
            caption,
            center_x,
            center_y + 20,
            DETAIL_SIZE,
            SECONDARY,
            false,
        );
    }

    fn render_support(&self, canvas: &mut Canvas) {
        let center_y = (HEADER_HEIGHT + DISPLAY_HEIGHT as i32) / 2 - 6;
        let card_y = center_y - SUPPORT_CARD_SIZE / 2;
        canvas.rounded_rect(
            SUPPORT_CARD_X,
            card_y,
            SUPPORT_CARD_SIZE,
            SUPPORT_CARD_SIZE,
            GROUP_RADIUS,
            WHITE,
            255,
        );
        let scale = (SUPPORT_CARD_SIZE - 32) / SUPPORT_QR_MODULES;
        let inset = (SUPPORT_CARD_SIZE - SUPPORT_QR_MODULES * scale) / 2;
        for (row, bits) in (0..).zip(SUPPORT_QR) {
            for column in 0..SUPPORT_QR_MODULES {
                if bits >> (SUPPORT_QR_MODULES - 1 - column) & 1 == 1 {
                    canvas.rounded_rect(
                        SUPPORT_CARD_X + inset + column * scale,
                        card_y + inset + row * scale,
                        scale,
                        scale,
                        0,
                        [0, 0, 0],
                        255,
                    );
                }
            }
        }
        let width = CONTENT_RIGHT - SUPPORT_TEXT_X;
        let lines = [
            ("Scan to donate", -70, LABEL_SIZE, WHITE, true),
            (
                "Donations are split between",
                -22,
                DETAIL_SIZE,
                SECONDARY,
                false,
            ),
            (
                "the Nocturne team and fund",
                6,
                DETAIL_SIZE,
                SECONDARY,
                false,
            ),
            (
                "future features. Thank you!",
                34,
                DETAIL_SIZE,
                SECONDARY,
                false,
            ),
            ("usenocturne.com/support", 82, DETAIL_SIZE, WHITE, true),
        ];
        for (text, offset, size, color, semibold) in lines {
            let text = self.fonts.fit(text, size, semibold, width);
            canvas.text(
                self.fonts,
                &text,
                SUPPORT_TEXT_X,
                center_y + offset,
                size,
                color,
                semibold,
            );
        }
    }

    fn render_layout(&self, canvas: &mut Canvas, layout: &Layout) {
        for &(top, bottom) in &layout.groups {
            canvas.rounded_rect(
                GROUP_X,
                top,
                GROUP_WIDTH,
                bottom - top,
                GROUP_RADIUS,
                GROUP,
                255,
            );
        }
        for (caption, y) in &layout.captions {
            canvas.text(
                self.fonts,
                caption,
                CONTENT_X,
                *y,
                CAPTION_SIZE,
                SECONDARY,
                true,
            );
        }
        for &(footer, y) in &layout.footers {
            canvas.text(
                self.fonts,
                footer,
                CONTENT_X,
                y,
                CAPTION_SIZE,
                SECONDARY,
                false,
            );
        }
        for (index, row) in layout.rows.iter().enumerate() {
            let focused = index == self.focus;
            if !row.first && !focused && index != self.focus + 1 {
                canvas.rounded_rect(
                    CONTENT_X,
                    row.top,
                    GROUP_X + GROUP_WIDTH - CONTENT_X,
                    1,
                    0,
                    DIVIDER,
                    255,
                );
            }
            if focused {
                self.render_focus(canvas, row);
            }
            if row.item == Item::DensitySlider {
                self.render_density_slider(canvas, row, focused);
            } else {
                self.render_row(canvas, row);
            }
        }
    }

    fn render_focus(&self, canvas: &mut Canvas, row: &Row) {
        let (x, y) = (GROUP_X + 4, row.top + 4);
        let (width, height) = (GROUP_WIDTH - 8, row.item.height() - 8);
        let radius = GROUP_RADIUS - 4;
        canvas.rounded_rect(x, y, width, height, radius, FOCUS, 255);
        let ring = if self.editing {
            Some(GREEN)
        } else if self.confirming_forget && row.item == Item::Forget {
            Some(RED)
        } else {
            None
        };
        if let Some(color) = ring {
            canvas.ring(x, y, width, height, radius, 2.0, color);
        }
    }

    fn render_row(&self, canvas: &mut Canvas, row: &Row) {
        let view = self.row_view(row.item);
        let center = row.top + row.item.height() / 2;
        let mut right = CONTENT_RIGHT;
        match &view.trailing {
            Trailing::None => {}
            Trailing::Switch(on) => {
                canvas.switch(CONTENT_RIGHT - SWITCH_WIDTH, center, *on);
                right -= SWITCH_WIDTH + CONTROL_GAP;
            }
            Trailing::Value(value, color) | Trailing::Link(value, color) => {
                if matches!(view.trailing, Trailing::Link(..)) {
                    canvas.chevron(CONTENT_RIGHT - 2, center, 10, true, SECONDARY, 3.0);
                    right -= 26;
                }
                if !value.is_empty() {
                    let value = self.fonts.fit(value, VALUE_SIZE, false, 360);
                    canvas.text_right(self.fonts, &value, right, center, VALUE_SIZE, *color, false);
                    right -= self.fonts.text_width(&value, VALUE_SIZE, false) + CONTROL_GAP;
                }
            }
        }
        let label = self
            .fonts
            .fit(&view.label, LABEL_SIZE, false, right - CONTENT_X);
        let label_y = if view.detail.is_some() {
            center - 14
        } else {
            center
        };
        canvas.text(
            self.fonts,
            &label,
            CONTENT_X,
            label_y,
            LABEL_SIZE,
            view.label_color,
            false,
        );
        if let Some(detail) = &view.detail {
            let detail = self
                .fonts
                .fit(detail, DETAIL_SIZE, false, right - CONTENT_X);
            canvas.text(
                self.fonts,
                &detail,
                CONTENT_X,
                center + 20,
                DETAIL_SIZE,
                SECONDARY,
                false,
            );
        }
    }

    fn render_density_slider(&self, canvas: &mut Canvas, row: &Row, focused: bool) {
        let active = focused && (self.editing || self.touch.is_some_and(|touch| touch.sliding));
        let label_y = row.top + 42;
        canvas.text(
            self.fonts, "Density", CONTENT_X, label_y, LABEL_SIZE, WHITE, false,
        );
        canvas.text_right(
            self.fonts,
            &format!("{} dpi", self.staged_dpi),
            CONTENT_RIGHT,
            label_y,
            VALUE_SIZE,
            if active { GREEN } else { SECONDARY },
            false,
        );
        let track_y = row.top + SLIDER_TRACK_OFFSET;
        let x = dpi_x(self.staged_dpi);
        canvas.rounded_rect(SLIDER_X, track_y - 4, SLIDER_WIDTH, 8, 4, TRACK, 255);
        canvas.rounded_rect(
            SLIDER_X,
            track_y - 4,
            (x - SLIDER_X).max(8),
            8,
            4,
            if active { GREEN } else { WHITE },
            255,
        );
        let default_x = dpi_x(self.settings.original_dpi);
        canvas.rounded_rect(default_x - 1, track_y - 12, 2, 24, 1, SECONDARY, 255);
        canvas.circle(x, track_y, 16, WHITE, 255);
        let caption_y = row.top + 116;
        canvas.text(
            self.fonts,
            "Smaller",
            CONTENT_X,
            caption_y,
            CAPTION_SIZE,
            SECONDARY,
            false,
        );
        canvas.text_right(
            self.fonts,
            "Larger",
            CONTENT_RIGHT,
            caption_y,
            CAPTION_SIZE,
            SECONDARY,
            false,
        );
    }

    fn option_trailing(&self, item: Item, value: &str) -> Trailing {
        if self.editing && self.focused_item() == Some(item) {
            Trailing::Value(format!("‹  {value}  ›"), GREEN)
        } else {
            Trailing::Value(value.to_owned(), SECONDARY)
        }
    }

    fn row_view(&self, item: Item) -> RowView {
        let on_off = |on: bool| if on { "On" } else { "Off" }.to_owned();
        match item {
            Item::Phones => {
                let active = self.phones.iter().find(|phone| self.is_active(phone));
                let (summary, color) = match (active, self.phones.len()) {
                    _ if !self.wireless => ("Off".to_owned(), SECONDARY),
                    (Some(phone), _) => (phone.name.clone(), GREEN),
                    (None, 0) => ("None".to_owned(), SECONDARY),
                    (None, count) => (format!("{count} saved"), SECONDARY),
                };
                RowView::new("Phones", Trailing::Link(summary, color))
            }
            Item::Wifi => {
                let summary = match self.staged_radio.filter(|_| self.wifi_available()) {
                    Some((channel, _)) => {
                        format!("{} · Ch {channel}", band_name(Band::of_channel(channel)))
                    }
                    None => "Off".to_owned(),
                };
                RowView::new("Wi-Fi", Trailing::Link(summary, SECONDARY))
            }
            Item::AutoBrightness => RowView::new(
                "Auto brightness",
                Trailing::Switch(self.settings.auto_brightness),
            ),
            Item::AutoNightMode => RowView::new(
                "Auto night mode",
                Trailing::Switch(self.settings.auto_night_mode),
            ),
            Item::Density => RowView::new(
                "Screen density",
                Trailing::Link(format!("{} dpi", self.staged_dpi), SECONDARY),
            ),
            Item::CarPlayDock => RowView::new(
                "CarPlay dock",
                self.option_trailing(item, self.staged_dock.label()),
            ),
            Item::PhoneAudio => {
                RowView::new("Phone audio", Trailing::Switch(self.staged_phone_audio))
                    .detail("Keep sound on your phone")
            }
            Item::Equalizer => RowView::new(
                "Equalizer",
                Trailing::Link(on_off(self.equalizer_ui.settings.enabled), SECONDARY),
            ),
            Item::Presets => {
                let summary = if self.staged_presets == PresetAction::DEFAULTS {
                    "Default"
                } else {
                    "Custom"
                };
                RowView::new(
                    "Preset buttons",
                    Trailing::Link(summary.to_owned(), SECONDARY),
                )
            }
            Item::Preset(index) => {
                let action = self
                    .staged_presets
                    .get(index)
                    .copied()
                    .unwrap_or(PresetAction::None);
                RowView::new(
                    format!("Button {}", index + 1),
                    self.option_trailing(item, action.label()),
                )
            }
            Item::PresetsReset => RowView::new("Reset to default", Trailing::None).color(
                if self.staged_presets == PresetAction::DEFAULTS {
                    SECONDARY
                } else {
                    WHITE
                },
            ),
            Item::Support => {
                RowView::new("Support Prelude", Trailing::Link(String::new(), SECONDARY))
            }
            Item::Version => {
                RowView::new("Version", Trailing::Value(self.version.clone(), SECONDARY))
            }
            Item::Phone(index) => {
                let Some(phone) = self.phones.get(index) else {
                    return RowView::new("", Trailing::None);
                };
                let (status, color) = if self.is_active(phone) {
                    ("Connected", GREEN)
                } else if self.is_pending(phone) {
                    ("Connecting…", SECONDARY)
                } else {
                    ("", SECONDARY)
                };
                RowView::new(phone.name.clone(), Trailing::Link(status.to_owned(), color))
            }
            Item::Connect => {
                let active = self.opened().is_some_and(|phone| self.is_active(phone));
                RowView::new(
                    if active { "Disconnect" } else { "Connect" },
                    Trailing::None,
                )
            }
            Item::Forget => RowView::new(
                if self.confirming_forget {
                    "Press again to forget"
                } else {
                    "Forget this phone"
                },
                Trailing::None,
            )
            .color(RED),
            Item::DensityReset => {
                let original = self.settings.original_dpi;
                let at_default = self.staged_dpi == original && !self.staged_dpi_customized;
                RowView::new(
                    "Reset to default",
                    Trailing::Value(format!("{original} dpi"), SECONDARY),
                )
                .color(if at_default { SECONDARY } else { WHITE })
            }
            Item::WifiBand | Item::WifiChannel | Item::WifiWidth => {
                let (channel, width) = self.staged_radio.unwrap_or((0, ChannelWidth::Mhz20));
                let (label, value) = match item {
                    Item::WifiBand => ("Band", band_name(Band::of_channel(channel)).to_owned()),
                    Item::WifiChannel => ("Channel", channel.to_string()),
                    _ => ("Channel width", format!("{} MHz", width.mhz())),
                };
                RowView::new(label, self.option_trailing(item, &value))
            }
            Item::DensitySlider => RowView::new("Density", Trailing::None),
        }
    }
}

fn system_version() -> String {
    fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|contents| os_release_version(&contents))
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned())
}

fn os_release_version(contents: &str) -> Option<String> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("VERSION_ID="))
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty())
}

const fn band_name(band: Band) -> &'static str {
    match band {
        Band::Ghz2_4 => "2.4 GHz",
        Band::Ghz5 => "5 GHz",
    }
}

fn step_option<T: Copy + PartialEq>(options: &[T], current: T, delta: isize, wrap: bool) -> T {
    let last = options.len().saturating_sub(1) as isize;
    let index = options
        .iter()
        .position(|&option| option == current)
        .unwrap_or(0) as isize;
    let next = if wrap {
        (index + delta).rem_euclid(last + 1)
    } else {
        (index + delta).clamp(0, last)
    };
    options.get(next as usize).copied().unwrap_or(current)
}

fn step_radio(
    (channel, width): (u8, ChannelWidth),
    item: Item,
    delta: isize,
    wrap: bool,
) -> (u8, ChannelWidth) {
    let band = Band::of_channel(channel);
    let channel = match item {
        Item::WifiBand => match step_option(&BANDS, band, delta, wrap) {
            next if next == band => channel,
            next => next.default_channel(),
        },
        Item::WifiChannel => step_option(band.channels(), channel, delta, wrap),
        _ => channel,
    };
    let width = match item {
        Item::WifiWidth => step_option(ChannelWidth::supported_on(channel), width, delta, wrap),
        _ => width,
    };
    (channel, width.min(ChannelWidth::widest_for(channel)))
}

fn selection_after_rotary(
    selected: usize,
    direction: RotaryDirection,
    steps: u8,
    last: usize,
) -> usize {
    let delta = usize::from(steps);
    match direction {
        RotaryDirection::Clockwise => selected.saturating_add(delta).min(last),
        RotaryDirection::CounterClockwise => selected.saturating_sub(delta),
    }
}

fn dpi_from_x(x: i32) -> u32 {
    let position = (x - SLIDER_X).clamp(0, SLIDER_WIDTH);
    let raw = MIN_DPI
        + ((u32::try_from(position).unwrap_or(0) * (MAX_DPI - MIN_DPI))
            / u32::try_from(SLIDER_WIDTH).unwrap_or(1));
    ((raw + DPI_STEP / 2) / DPI_STEP * DPI_STEP).clamp(MIN_DPI, MAX_DPI)
}

fn dpi_x(dpi: u32) -> i32 {
    let offset = dpi.clamp(MIN_DPI, MAX_DPI) - MIN_DPI;
    SLIDER_X + (i64::from(offset) * i64::from(SLIDER_WIDTH) / i64::from(MAX_DPI - MIN_DPI)) as i32
}

struct Canvas {
    rgb: Vec<u8>,
    offset_y: i32,
    clip_top: i32,
}

impl Canvas {
    fn new() -> Self {
        Self {
            rgb: vec![0; DISPLAY_WIDTH * DISPLAY_HEIGHT * 3],
            offset_y: 0,
            clip_top: 0,
        }
    }

    fn scroll(&mut self, offset_y: i32, clip_top: i32) {
        self.offset_y = offset_y;
        self.clip_top = clip_top;
    }

    fn visible_rows(&self) -> std::ops::Range<i32> {
        self.offset_y + self.clip_top..self.offset_y + DISPLAY_HEIGHT as i32
    }

    fn fill(
        &mut self,
        (left, top, right, bottom): (i32, i32, i32, i32),
        color: [u8; 3],
        alpha: u8,
        distance: impl Fn(f32, f32) -> f32,
    ) {
        let rows = self.visible_rows();
        for py in top.max(rows.start)..bottom.min(rows.end) {
            for px in left.max(0)..right.min(DISPLAY_WIDTH as i32) {
                let coverage = (0.5 - distance(px as f32 + 0.5, py as f32 + 0.5)).clamp(0.0, 1.0);
                if coverage > 0.0 {
                    self.blend(px, py, color, (f32::from(alpha) * coverage).round() as u8);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn rounded_rect(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        radius: i32,
        color: [u8; 3],
        alpha: u8,
    ) {
        let distance = rounded_rect_distance(x, y, width, height, radius);
        self.fill((x, y, x + width, y + height), color, alpha, distance);
    }

    #[allow(clippy::too_many_arguments)]
    fn ring(
        &mut self,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        radius: i32,
        thickness: f32,
        color: [u8; 3],
    ) {
        let distance = rounded_rect_distance(x, y, width, height, radius);
        let half = thickness / 2.0;
        self.fill((x, y, x + width, y + height), color, 255, |px, py| {
            (distance(px, py) + half).abs() - half
        });
    }

    fn circle(&mut self, center_x: i32, center_y: i32, radius: i32, color: [u8; 3], alpha: u8) {
        let (cx, cy, r) = (center_x as f32, center_y as f32, radius as f32);
        self.fill(
            (
                center_x - radius - 1,
                center_y - radius - 1,
                center_x + radius + 1,
                center_y + radius + 1,
            ),
            color,
            alpha,
            |px, py| (px - cx).hypot(py - cy) - r,
        );
    }

    fn line(&mut self, from: (f32, f32), to: (f32, f32), width: f32, color: [u8; 3]) {
        let half = width / 2.0;
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        let length = (dx * dx + dy * dy).max(f32::EPSILON);
        let bounds = (
            (from.0.min(to.0) - half).floor() as i32 - 1,
            (from.1.min(to.1) - half).floor() as i32 - 1,
            (from.0.max(to.0) + half).ceil() as i32 + 1,
            (from.1.max(to.1) + half).ceil() as i32 + 1,
        );
        self.fill(bounds, color, 255, |px, py| {
            let t = (((px - from.0) * dx + (py - from.1) * dy) / length).clamp(0.0, 1.0);
            (px - from.0 - t * dx).hypot(py - from.1 - t * dy) - half
        });
    }

    fn chevron(
        &mut self,
        tip_x: i32,
        center_y: i32,
        half_height: i32,
        right: bool,
        color: [u8; 3],
        width: f32,
    ) {
        let (tip_x, center_y, half) = (tip_x as f32, center_y as f32, half_height as f32);
        let arm_x = if right {
            tip_x - half * 0.75
        } else {
            tip_x + half * 0.75
        };
        let tip = (tip_x, center_y);
        self.line((arm_x, center_y - half), tip, width, color);
        self.line(tip, (arm_x, center_y + half), width, color);
    }

    fn switch(&mut self, x: i32, center_y: i32, on: bool) {
        let top = center_y - SWITCH_HEIGHT / 2;
        self.rounded_rect(
            x,
            top,
            SWITCH_WIDTH,
            SWITCH_HEIGHT,
            SWITCH_HEIGHT / 2,
            if on { GREEN } else { TRACK },
            255,
        );
        let thumb_x = if on {
            x + SWITCH_WIDTH - SWITCH_HEIGHT / 2
        } else {
            x + SWITCH_HEIGHT / 2
        };
        self.circle(thumb_x, center_y, SWITCH_HEIGHT / 2 - 4, WHITE, 255);
    }

    #[allow(clippy::too_many_arguments)]
    fn text(
        &mut self,
        fonts: &FontSet,
        text: &str,
        x: i32,
        center_y: i32,
        size: f32,
        color: [u8; 3],
        semibold: bool,
    ) {
        fonts.draw(text, (x, center_y), size, semibold, |px, py, alpha| {
            self.blend(px, py, color, alpha);
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn text_right(
        &mut self,
        fonts: &FontSet,
        text: &str,
        right: i32,
        center_y: i32,
        size: f32,
        color: [u8; 3],
        semibold: bool,
    ) {
        let width = fonts.text_width(text, size, semibold);
        self.text(fonts, text, right - width, center_y, size, color, semibold);
    }

    #[allow(clippy::too_many_arguments)]
    fn text_center(
        &mut self,
        fonts: &FontSet,
        text: &str,
        center_x: i32,
        center_y: i32,
        size: f32,
        color: [u8; 3],
        semibold: bool,
    ) {
        let width = fonts.text_width(text, size, semibold);
        self.text(
            fonts,
            text,
            center_x - width / 2,
            center_y,
            size,
            color,
            semibold,
        );
    }
    fn blend(&mut self, x: i32, y: i32, color: [u8; 3], alpha: u8) {
        let y = y - self.offset_y;
        if !(0..DISPLAY_WIDTH as i32).contains(&x)
            || !(self.clip_top..DISPLAY_HEIGHT as i32).contains(&y)
        {
            return;
        }
        let offset = (y as usize * DISPLAY_WIDTH + x as usize) * 3;
        let alpha = u16::from(alpha);
        let inverse = 255_u16.saturating_sub(alpha);
        for (channel, &foreground) in color.iter().enumerate() {
            self.rgb[offset + channel] = ((u16::from(self.rgb[offset + channel]) * inverse
                + u16::from(foreground) * alpha
                + 127)
                / 255) as u8;
        }
    }

    fn to_nv12(&self) -> Vec<u8> {
        let mut frame = vec![0_u8; DISPLAY_WIDTH * DISPLAY_HEIGHT * 3 / 2];
        let luma_size = DISPLAY_WIDTH * DISPLAY_HEIGHT;
        for y in 0..DISPLAY_HEIGHT {
            for x in 0..DISPLAY_WIDTH {
                let offset = (y * DISPLAY_WIDTH + x) * 3;
                frame[y * DISPLAY_WIDTH + x] =
                    rgb_to_yuv(self.rgb[offset], self.rgb[offset + 1], self.rgb[offset + 2]).0;
            }
        }
        for y in (0..DISPLAY_HEIGHT).step_by(2) {
            for x in (0..DISPLAY_WIDTH).step_by(2) {
                let mut u = 0_u16;
                let mut v = 0_u16;
                for sample_y in y..y + 2 {
                    for sample_x in x..x + 2 {
                        let offset = (sample_y * DISPLAY_WIDTH + sample_x) * 3;
                        let (_, sample_u, sample_v) = rgb_to_yuv(
                            self.rgb[offset],
                            self.rgb[offset + 1],
                            self.rgb[offset + 2],
                        );
                        u += u16::from(sample_u);
                        v += u16::from(sample_v);
                    }
                }
                let uv_offset = luma_size + (y / 2) * DISPLAY_WIDTH + x;
                frame[uv_offset] = ((u + 2) / 4) as u8;
                frame[uv_offset + 1] = ((v + 2) / 4) as u8;
            }
        }
        frame
    }
}

fn rgb_to_yuv(red: u8, green: u8, blue: u8) -> (u8, u8, u8) {
    let red = i32::from(red);
    let green = i32::from(green);
    let blue = i32::from(blue);
    let clamp = |value: i32| value.clamp(0, 255) as u8;
    (
        clamp(((47 * red + 157 * green + 16 * blue + 128) >> 8) + 16),
        clamp(((-26 * red - 86 * green + 112 * blue + 128) >> 8) + 128),
        clamp(((112 * red - 102 * green - 10 * blue + 128) >> 8) + 128),
    )
}

fn rounded_rect_distance(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    radius: i32,
) -> impl Fn(f32, f32) -> f32 {
    let (half_width, half_height) = (width as f32 / 2.0, height as f32 / 2.0);
    let radius = (radius as f32).min(half_width).min(half_height);
    let (center_x, center_y) = (x as f32 + half_width, y as f32 + half_height);
    let (inner_x, inner_y) = (half_width - radius, half_height - radius);
    move |px, py| {
        let qx = (px - center_x).abs() - inner_x;
        let qy = (py - center_y).abs() - inner_y;
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius
    }
}

#[cfg(test)]
mod tests {
    use platform_superbird::MonotonicMicros;
    use prelude_wireless::bluez::PhoneKind;

    use super::*;
    use crate::fonts::bitmap_glyph;

    struct NullSurface;

    impl OverlaySurface for NullSurface {
        fn set_overlay(&self, _frame: Vec<u8>) -> prelude_media::Result<()> {
            Ok(())
        }

        fn clear_overlay(&self) -> prelude_media::Result<()> {
            Ok(())
        }

        fn set_host_ui_active(&self, _active: bool) -> prelude_media::Result<()> {
            Ok(())
        }
    }

    struct Harness {
        ui: SettingsUi<NullSurface>,
        events: Receiver<HostUiEvent>,
        _state: tempfile::TempDir,
    }

    impl Harness {
        fn new() -> Self {
            let state = tempfile::TempDir::new().expect("state directory");
            let (sender, events) = mpsc::channel();
            let settings = RuntimeSettings {
                wifi_channel: Some(36),
                wifi_channel_width_mhz: Some(20),
                ..RuntimeSettings::default()
            };
            let mut ui = SettingsUi::new(
                NullSurface,
                state.path().to_path_buf(),
                settings,
                prelude_media::EqualizerControl::default(),
                PresetBindings::default(),
                None,
                None,
                sender,
                Arc::new(Notify::new()),
            );
            ui.open();
            Self {
                ui,
                events,
                _state: state,
            }
        }

        fn with_phones(mut self) -> Self {
            self.ui.wireless = true;
            self.ui.phones = vec![
                KnownPhone {
                    kind: PhoneKind::CarPlay,
                    address: "aa".to_owned(),
                    name: "Neel's iPhone".to_owned(),
                },
                KnownPhone {
                    kind: PhoneKind::AndroidAuto,
                    address: "bb".to_owned(),
                    name: "Pixel 9 Pro".to_owned(),
                },
            ];
            self.ui.active_phone = Some("aa".to_owned());
            self
        }

        fn send(&mut self, kind: PlatformInputEventKind) {
            self.ui.handle(PlatformInputEvent {
                timestamp: MonotonicMicros(0),
                kind,
            });
        }

        fn press(&mut self, button: KnobInputButton) {
            self.send(PlatformInputEventKind::KnobButton {
                button,
                pressed: false,
            });
        }

        fn turn(&mut self, steps: i8) {
            self.send(PlatformInputEventKind::Rotary {
                direction: if steps >= 0 {
                    RotaryDirection::Clockwise
                } else {
                    RotaryDirection::CounterClockwise
                },
                steps: steps.unsigned_abs(),
            });
        }

        fn touch(&mut self, phase: TouchInputPhase, x: i32, y: i32) {
            self.send(PlatformInputEventKind::Touch {
                contact_id: 0,
                phase,
                x: x as u16,
                y: y as u16,
            });
        }

        fn tap(&mut self, x: i32, y: i32) {
            self.touch(TouchInputPhase::Down, x, y);
            self.touch(TouchInputPhase::Up, x, y);
        }

        fn focus(&mut self, item: Item) {
            let index = self
                .ui
                .layout()
                .rows
                .iter()
                .position(|row| row.item == item)
                .expect("item is on the page");
            let delta = index as i8 - self.ui.focus as i8;
            self.turn(delta);
            assert_eq!(self.ui.focused_item(), Some(item));
        }

        fn screen_y(&self, item: Item) -> i32 {
            let row = self
                .ui
                .layout()
                .rows
                .into_iter()
                .find(|row| row.item == item)
                .expect("item is on the page");
            row.top + row.item.height() / 2 - self.ui.scroll
        }
    }

    #[test]
    fn version_comes_from_os_release() {
        assert_eq!(
            os_release_version("ID=prelude\nVERSION_ID=\"1.0.0\"\n").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(os_release_version("VERSION_ID=\"\"\n"), None);
        assert_eq!(os_release_version("ID=prelude\n"), None);
    }

    #[test]
    fn dpi_slider_clamps_and_snaps_to_ten() {
        assert_eq!(dpi_from_x(0), MIN_DPI);
        assert_eq!(dpi_from_x(SLIDER_X + SLIDER_WIDTH), MAX_DPI);
        assert_eq!(dpi_from_x(SLIDER_X + SLIDER_WIDTH / 2), 290);
        for dpi in (MIN_DPI..=MAX_DPI).step_by(DPI_STEP as usize) {
            assert_eq!(dpi_from_x(dpi_x(dpi)), dpi);
        }
    }

    #[test]
    fn settings_sections_stack_without_overlap_and_scroll_to_the_last_row() {
        let harness = Harness::new();
        let layout = harness.ui.layout();
        assert_eq!(layout.groups.len(), 5);
        assert_eq!(layout.captions.len(), 4);
        for pair in layout.rows.windows(2) {
            assert!(pair[0].bottom() <= pair[1].top, "{pair:?}");
        }
        let last = layout.rows.last().expect("rows");
        assert_eq!(last.item, Item::Version);
        assert_eq!(
            last.bottom() + SECTION_GAP - layout.maximum_scroll(),
            DISPLAY_HEIGHT as i32
        );
    }

    #[test]
    fn rotary_walks_every_row_and_keeps_the_focus_on_screen() {
        let mut harness = Harness::new();
        let rows = harness.ui.layout().rows;
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(harness.ui.focus, index);
            let top = row.top - harness.ui.scroll;
            assert!(
                top >= HEADER_HEIGHT,
                "{:?} hidden under the header",
                row.item
            );
            assert!(
                row.bottom() - harness.ui.scroll <= DISPLAY_HEIGHT as i32,
                "{:?} hidden below the screen",
                row.item
            );
            harness.turn(1);
        }
        assert_eq!(
            harness.ui.focus,
            rows.len() - 1,
            "rotation stops at the end"
        );
        harness.turn(-100);
        assert_eq!((harness.ui.focus, harness.ui.scroll), (0, 0));
    }

    #[test]
    fn tapping_rows_toggles_and_navigates() {
        let mut harness = Harness::new();
        let y = harness.screen_y(Item::AutoBrightness);
        harness.tap(400, y);
        assert!(!harness.ui.settings.auto_brightness);
        harness.tap(400, harness.screen_y(Item::Density));
        assert_eq!(harness.ui.page, Page::Density);
        harness.tap(BACK_X, TITLE_Y);
        assert_eq!(harness.ui.page, Page::Settings);
        assert_eq!(harness.ui.focused_item(), Some(Item::Density));
    }

    #[test]
    fn dragging_scrolls_without_activating_the_touched_row() {
        let mut harness = Harness::new();
        let y = harness.screen_y(Item::AutoNightMode);
        harness.touch(TouchInputPhase::Down, 400, y);
        harness.touch(TouchInputPhase::Move, 400, y - 80);
        harness.touch(TouchInputPhase::Up, 400, y - 80);
        assert_eq!(harness.ui.scroll, 80);
        assert!(harness.ui.settings.auto_night_mode);
    }

    #[test]
    fn density_is_staged_with_the_knob_and_reset_restores_the_original() {
        let mut harness = Harness::new();
        harness.focus(Item::Density);
        harness.press(KnobInputButton::Select);
        assert_eq!(harness.ui.page, Page::Density);
        harness.press(KnobInputButton::Select);
        assert!(harness.ui.editing);
        harness.turn(3);
        assert_eq!(harness.ui.staged_dpi, harness.ui.settings.reported_dpi + 30);
        assert!(harness.ui.has_staged_changes());
        harness.press(KnobInputButton::Back);
        assert!(!harness.ui.editing);
        harness.turn(1);
        harness.press(KnobInputButton::Select);
        assert_eq!(harness.ui.staged_dpi, harness.ui.settings.original_dpi);
        assert!(!harness.ui.has_staged_changes());
        harness.press(KnobInputButton::Back);
        assert_eq!(harness.ui.page, Page::Settings);
        assert_eq!(harness.ui.focused_item(), Some(Item::Density));
    }

    #[test]
    fn density_slider_follows_a_horizontal_drag_but_ignores_label_taps() {
        let mut harness = Harness::new();
        harness.ui.enter(Page::Density);
        let original = harness.ui.staged_dpi;
        let label_y = HEADER_HEIGHT + 42;
        harness.tap(SLIDER_X + SLIDER_WIDTH, label_y);
        assert_eq!(harness.ui.staged_dpi, original);
        harness.touch(TouchInputPhase::Down, dpi_x(original), label_y);
        harness.touch(TouchInputPhase::Move, SLIDER_X + SLIDER_WIDTH + 40, label_y);
        harness.touch(TouchInputPhase::Up, SLIDER_X + SLIDER_WIDTH + 40, label_y);
        assert_eq!(harness.ui.staged_dpi, MAX_DPI);
        harness.tap(SLIDER_X, HEADER_HEIGHT + SLIDER_TRACK_OFFSET);
        assert_eq!(harness.ui.staged_dpi, MIN_DPI);
    }

    #[test]
    fn closing_commits_staged_changes_once() {
        let mut harness = Harness::new();
        harness.focus(Item::PhoneAudio);
        harness.press(KnobInputButton::Select);
        assert!(harness.ui.staged_phone_audio);
        harness.press(KnobInputButton::Back);
        assert!(!harness.ui.menu_open);
        let events: Vec<_> = harness.events.try_iter().collect();
        assert!(matches!(
            events.as_slice(),
            [HostUiEvent::SettingsCommitted(SettingsCommitted {
                restart_required: true,
                radio_changed: false,
                ..
            })]
        ));
        assert!(harness.ui.settings.phone_audio);
    }

    #[test]
    fn carplay_dock_is_picked_in_place_and_restarts_projection_on_close() {
        let mut harness = Harness::new();
        harness.focus(Item::CarPlayDock);
        harness.press(KnobInputButton::Select);
        assert!(harness.ui.editing);
        harness.turn(2);
        assert_eq!(harness.ui.staged_dock, CarPlayDock::Bottom);
        harness.turn(1);
        assert_eq!(harness.ui.staged_dock, CarPlayDock::Bottom);
        harness.turn(-1);
        assert_eq!(harness.ui.staged_dock, CarPlayDock::Right);
        assert!(harness.ui.has_staged_changes());
        harness.press(KnobInputButton::Back);
        assert!(harness.ui.menu_open);
        harness.tap(400, harness.screen_y(Item::CarPlayDock));
        assert_eq!(harness.ui.staged_dock, CarPlayDock::Bottom);
        harness.tap(400, harness.screen_y(Item::CarPlayDock));
        assert_eq!(harness.ui.staged_dock, CarPlayDock::Left);
        harness.tap(400, harness.screen_y(Item::CarPlayDock));
        assert_eq!(harness.ui.staged_dock, CarPlayDock::Right);
        harness.press(KnobInputButton::Back);
        assert!(!harness.ui.menu_open);
        assert!(matches!(
            harness.events.try_iter().collect::<Vec<_>>().as_slice(),
            [HostUiEvent::SettingsCommitted(SettingsCommitted {
                restart_required: true,
                radio_changed: false,
                settings: RuntimeSettings {
                    carplay_dock: CarPlayDock::Right,
                    ..
                },
            })]
        ));
        assert_eq!(harness.ui.settings.carplay_dock, CarPlayDock::Right);
    }

    #[test]
    fn preset_buttons_are_remapped_live_on_close_without_a_projection_restart() {
        let mut harness = Harness::new();
        let live = harness.ui.presets.clone();
        harness.focus(Item::Presets);
        harness.press(KnobInputButton::Select);
        assert_eq!(harness.ui.page, Page::Presets);
        harness.focus(Item::Preset(0));
        harness.press(KnobInputButton::Select);
        harness.turn(4);
        assert_eq!(harness.ui.staged_presets[0], PresetAction::Home);
        harness.turn(-6);
        assert_eq!(harness.ui.staged_presets[0], PresetAction::PreviousTrack);
        harness.turn(-1);
        harness.press(KnobInputButton::Back);
        harness.tap(400, harness.screen_y(Item::Preset(3)));
        assert_eq!(harness.ui.staged_presets[3], PresetAction::Home);
        assert_eq!(live.actions(), PresetAction::DEFAULTS);
        harness.focus(Item::PresetsReset);
        harness.press(KnobInputButton::Select);
        assert_eq!(harness.ui.staged_presets, PresetAction::DEFAULTS);
        harness.tap(400, harness.screen_y(Item::Preset(3)));
        harness.press(KnobInputButton::Back);
        assert_eq!(harness.ui.page, Page::Settings);
        assert!(harness.ui.has_staged_changes());
        harness.press(KnobInputButton::Back);
        assert!(!harness.ui.menu_open);
        let expected = [
            PresetAction::PreviousTrack,
            PresetAction::PlayPause,
            PresetAction::NextTrack,
            PresetAction::Home,
        ];
        assert_eq!(live.actions(), expected);
        assert_eq!(harness.ui.settings.presets, expected);
        assert!(harness.events.try_iter().next().is_none());
    }

    #[test]
    fn wifi_values_are_edited_in_place_and_back_leaves_edit_mode_first() {
        let mut harness = Harness::new();
        harness.ui.wireless = true;
        harness.ui.enter(Page::Wifi);
        harness.press(KnobInputButton::Select);
        harness.turn(-1);
        assert_eq!(harness.ui.staged_radio, Some((6, ChannelWidth::Mhz20)));
        harness.press(KnobInputButton::Back);
        assert_eq!(harness.ui.page, Page::Wifi);
        harness.turn(1);
        harness.press(KnobInputButton::Select);
        harness.turn(2);
        assert_eq!(harness.ui.staged_radio, Some((8, ChannelWidth::Mhz20)));
        harness.turn(1);
        assert_eq!(harness.ui.staged_radio, Some((9, ChannelWidth::Mhz20)));
        assert_eq!(
            harness.ui.focus, 1,
            "rotation edits instead of moving focus"
        );
    }

    #[test]
    fn forgetting_a_phone_needs_a_second_press() {
        let mut harness = Harness::new().with_phones();
        harness.ui.enter(Page::Phones);
        harness.turn(1);
        harness.press(KnobInputButton::Select);
        assert_eq!(harness.ui.page, Page::Phone);
        assert_eq!(harness.ui.title(), "Pixel 9 Pro");
        harness.turn(1);
        harness.press(KnobInputButton::Select);
        assert!(harness.ui.confirming_forget);
        assert!(harness.events.try_iter().next().is_none());
        harness.press(KnobInputButton::Select);
        assert_eq!(
            harness.events.try_iter().collect::<Vec<_>>(),
            [HostUiEvent::ForgetPhone("bb".to_owned())]
        );
        assert_eq!(harness.ui.page, Page::Phones);
        assert_eq!(harness.ui.phones.len(), 1);
    }

    #[test]
    fn connected_phone_offers_disconnect() {
        let mut harness = Harness::new().with_phones();
        harness.ui.enter(Page::Phones);
        harness.press(KnobInputButton::Select);
        harness.press(KnobInputButton::Select);
        assert_eq!(
            harness.events.try_iter().collect::<Vec<_>>(),
            [HostUiEvent::DisconnectPhone("aa".to_owned())]
        );
    }

    #[test]
    fn wifi_band_switch_resets_the_channel_and_caps_the_width() {
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz20), Item::WifiBand, 1, true),
            (6, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz80), Item::WifiBand, -1, false),
            (6, ChannelWidth::Mhz40)
        );
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz20), Item::WifiBand, 1, false),
            (36, ChannelWidth::Mhz20)
        );
    }

    #[test]
    fn wifi_channels_stay_in_band() {
        assert_eq!(
            step_radio((161, ChannelWidth::Mhz80), Item::WifiChannel, 1, false),
            (165, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((165, ChannelWidth::Mhz20), Item::WifiChannel, 1, false),
            (165, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((165, ChannelWidth::Mhz20), Item::WifiChannel, 1, true),
            (36, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((48, ChannelWidth::Mhz20), Item::WifiChannel, 1, false),
            (149, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((1, ChannelWidth::Mhz20), Item::WifiChannel, 20, false),
            (13, ChannelWidth::Mhz20)
        );
    }

    #[test]
    fn wifi_width_only_offers_what_the_channel_supports() {
        assert_eq!(
            step_radio((36, ChannelWidth::Mhz20), Item::WifiWidth, 2, false),
            (36, ChannelWidth::Mhz80)
        );
        assert_eq!(
            step_radio((6, ChannelWidth::Mhz40), Item::WifiWidth, 1, true),
            (6, ChannelWidth::Mhz20)
        );
        assert_eq!(
            step_radio((165, ChannelWidth::Mhz20), Item::WifiWidth, 1, false),
            (165, ChannelWidth::Mhz20)
        );
    }

    #[test]
    fn scrolled_canvas_draws_rows_below_the_fixed_title() {
        let pixel = |canvas: &Canvas, y: usize| canvas.rgb[y * DISPLAY_WIDTH * 3];
        let mut canvas = Canvas::new();
        canvas.scroll(100, VIEWPORT_TOP);
        canvas.rounded_rect(0, 0, 10, 600, 0, WHITE, 255);
        assert_eq!(pixel(&canvas, VIEWPORT_TOP as usize - 1), 0);
        assert_eq!(pixel(&canvas, VIEWPORT_TOP as usize), 255);
        assert_eq!(pixel(&canvas, DISPLAY_HEIGHT - 1), 255);

        let mut canvas = Canvas::new();
        canvas.scroll(100, VIEWPORT_TOP);
        canvas.rounded_rect(0, 500, 10, 20, 0, WHITE, 255);
        assert_eq!(pixel(&canvas, 400), 255);
        assert_eq!(pixel(&canvas, 399), 0);
    }

    #[test]
    fn shapes_are_antialiased_at_their_edges() {
        let mut canvas = Canvas::new();
        canvas.circle(100, 100, 20, WHITE, 255);
        let red = |x: usize, y: usize| canvas.rgb[(y * DISPLAY_WIDTH + x) * 3];
        assert_eq!(red(100, 100), 255);
        assert_eq!(red(100, 70), 0);
        let edge = red(114, 113);
        assert!(edge > 0 && edge < 255, "edge coverage {edge}");
    }

    #[test]
    fn long_names_are_truncated_to_fit() {
        let fonts = FontSet {
            regular: None,
            semibold: None,
        };
        assert_eq!(fonts.fit("Pixel", LABEL_SIZE, true, 400), "Pixel");
        let fitted = fonts.fit("A very long phone name indeed", LABEL_SIZE, true, 200);
        assert!(fitted.ends_with('…'));
        assert!(fonts.text_width(&fitted, LABEL_SIZE, true) <= 200);
    }

    #[test]
    fn nv12_conversion_uses_limited_range_neutral_black_and_white() {
        assert_eq!(rgb_to_yuv(0, 0, 0), (16, 128, 128));
        assert_eq!(rgb_to_yuv(255, 255, 255), (235, 128, 128));
    }

    #[test]
    fn built_in_font_covers_settings_labels() {
        for character in
            "Settings Wireless Display Audio Phones Wi-Fi Auto brightness night mode Screen \
             density Phone audio Keep sound on your phone Equalizer On Off None saved Ch 2.4 5 GHz \
             Applies when settings close Unable to save apply equalizer Connected Connecting… \
             CarPlay Android Auto · Connect Disconnect Forget this phone Press again to forget \
             Density dpi Smaller Larger Reset to default CarPlay and Android Auto restart after \
             Band Channel width MHz ‹ › restarts Wireless is off Enable wireless projection \
             manage change No saved phones you connect wirelessly show up here Neel's \
             Reset flat Preamp +12.0 dB All USB output EQ curve unavailable bypasses 31 63 125 \
             250 500 1k 2k 4k 8k 16k −24 USB audio only Turn choose Press adjust finish Release \
             Controls Preset buttons Default Custom Button 1 2 3 4 Previous track Play/Pause Next \
             Mute Home Back Voice assistant Answer call End Reset to default Support Prelude \
             Scan to donate Donations are split between the team and fund future features. \
             Thank you! usenocturne.com/support"
                .chars()
        {
            assert!(
                bitmap_glyph(character).is_some(),
                "missing glyph {character:?}"
            );
        }
    }

    #[test]
    fn every_page_renders_a_full_nv12_frame() {
        let mut harness = Harness::new().with_phones();
        let preview = std::env::var("PRELUDE_SETTINGS_PREVIEW")
            .ok()
            .map(PathBuf::from);
        let capture = |harness: &Harness, name: &str| {
            let frame = harness.ui.render();
            assert_eq!(frame.len(), DISPLAY_WIDTH * DISPLAY_HEIGHT * 3 / 2);
            if let Some(directory) = preview.as_ref() {
                let mut canvas = Canvas::new();
                harness.ui.render_into(&mut canvas);
                let mut data = format!("P6\n{DISPLAY_WIDTH} {DISPLAY_HEIGHT}\n255\n").into_bytes();
                data.extend(&canvas.rgb);
                fs::create_dir_all(directory).expect("preview directory");
                fs::write(directory.join(format!("{name}.ppm")), data).expect("write preview");
            }
        };
        capture(&harness, "settings");
        harness.focus(Item::Density);
        capture(&harness, "settings-scrolled");
        harness.focus(Item::PhoneAudio);
        harness.press(KnobInputButton::Select);
        harness.focus(Item::Version);
        capture(&harness, "settings-bottom");
        harness.focus(Item::Support);
        harness.press(KnobInputButton::Select);
        assert_eq!(harness.ui.page, Page::Support);
        capture(&harness, "support");
        harness.press(KnobInputButton::Back);
        assert_eq!(harness.ui.focused_item(), Some(Item::Support));
        harness.focus(Item::Presets);
        harness.press(KnobInputButton::Select);
        harness.focus(Item::Preset(1));
        harness.press(KnobInputButton::Select);
        harness.turn(5);
        capture(&harness, "presets");
        harness.press(KnobInputButton::Back);
        harness.press(KnobInputButton::Back);
        harness.focus(Item::Equalizer);
        harness.press(KnobInputButton::Select);
        capture(&harness, "equalizer");
        harness.press(KnobInputButton::Back);
        harness.focus(Item::Phones);
        harness.press(KnobInputButton::Select);
        capture(&harness, "phones");
        harness.ui.pending_connect = Some("bb".to_owned());
        harness.turn(1);
        capture(&harness, "phones-connecting");
        harness.press(KnobInputButton::Select);
        harness.turn(1);
        harness.press(KnobInputButton::Select);
        capture(&harness, "phone-forget");
        harness.ui.phones.clear();
        harness.ui.go_back();
        capture(&harness, "phones-empty");
        harness.ui.go_back();
        harness.focus(Item::Wifi);
        harness.press(KnobInputButton::Select);
        harness.turn(1);
        harness.press(KnobInputButton::Select);
        capture(&harness, "wifi-editing");
        harness.ui.go_back();
        harness.ui.wireless = false;
        harness.press(KnobInputButton::Select);
        capture(&harness, "wifi-off");
        harness.ui.go_back();
        harness.focus(Item::Density);
        harness.press(KnobInputButton::Select);
        capture(&harness, "density");
        harness.press(KnobInputButton::Select);
        harness.turn(6);
        capture(&harness, "density-editing");
        harness.ui.error_message = Some("Unable to save settings");
        harness.ui.go_back();
        capture(&harness, "settings-error");
    }
}
