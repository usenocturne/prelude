//! Settings > Advanced > Internet: get online over Wi-Fi (or a wired
//! adapter), choose a network, and enter its password.

use prelude_wireless::station::Security;

use super::{
    GREEN, HEADER_HEIGHT, Item, KnobInputButton, OverlaySurface, Page, RED, RotaryDirection,
    RowView, SECONDARY, Section, SettingsUi, TouchInputPhase, Trailing, WHITE,
    keyboard::{Keyboard, KeyboardAction},
};
use crate::{
    internet::{InternetCommand, InternetError, InternetState, Route},
    saved_networks,
};

pub(super) const INTERNET_FOOTER: &str = "Wireless CarPlay and Android Auto pause while online";

impl<D: OverlaySurface> SettingsUi<D> {
    pub(super) const fn in_internet_pages(&self) -> bool {
        matches!(
            self.page,
            Page::Internet | Page::WifiNetworks | Page::Keyboard
        )
    }

    fn send_internet(&self, command: InternetCommand) {
        if let Some(internet) = self.internet.as_ref() {
            internet.send(command);
        }
    }

    /// Hands the adapter back to the access point when the user leaves.
    pub(super) fn release_internet(&mut self) {
        if self.internet.is_some() {
            self.send_internet(InternetCommand::Release);
            self.keyboard = None;
            self.awaiting_connect = false;
        }
    }

    /// Picks up worker state changes; returns whether the screen changed.
    pub(super) fn poll_internet(&mut self) -> bool {
        let Some(state) = self.internet.as_ref().map(crate::internet::Internet::state) else {
            return false;
        };
        if state == self.internet_state {
            return false;
        }
        if let InternetState::Networks(networks) = &state {
            self.networks.clone_from(networks);
        }
        let no_saved = state == InternetState::Failed(InternetError::NoSavedNetwork);
        self.internet_state = state;
        if no_saved && self.awaiting_connect && self.page == Page::Internet {
            self.awaiting_connect = false;
            self.open_networks();
            return true;
        }
        if !matches!(
            self.internet_state,
            InternetState::Pausing | InternetState::Scanning
        ) {
            self.awaiting_connect = false;
        }
        self.scroll = self.scroll.min(self.layout().maximum_scroll());
        self.focus = self.focus.min(self.layout().rows.len().saturating_sub(1));
        matches!(
            self.page,
            Page::Settings | Page::Internet | Page::WifiNetworks
        )
    }

    pub(super) fn internet_caption(&self) -> Option<String> {
        Some(match &self.internet_state {
            InternetState::Idle | InternetState::Networks(_) => return None,
            InternetState::Pausing => "Pausing wireless projection…".to_owned(),
            InternetState::Scanning => "Searching…".to_owned(),
            InternetState::Joining { ssid } => format!("Joining {ssid}…"),
            InternetState::Online {
                route: Route::Wired { .. },
                ..
            } => "Online via wired".to_owned(),
            InternetState::Online {
                route: Route::Wifi { ssid },
                address,
            } => format!("Online via {ssid} · {address}"),
            InternetState::Failed(error) => error.message().to_owned(),
        })
    }

    pub(super) fn internet_sections(&self) -> Vec<Section> {
        match self.page {
            Page::Internet => vec![
                Section {
                    title: self.internet_caption(),
                    ..Section::plain(vec![Item::InternetNetwork, Item::InternetTest])
                }
                .footer(INTERNET_FOOTER),
            ],
            Page::WifiNetworks if !self.networks.is_empty() => {
                let mut sections = vec![Section::plain(
                    (0..self.networks.len()).map(Item::Network).collect(),
                )];
                if !saved_networks::load(&self.state_dir).is_empty() {
                    sections.push(Section::plain(vec![Item::NetworkForget]));
                }
                sections
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn wifi_networks_empty_text(&self) -> (&'static str, &'static str) {
        match &self.internet_state {
            InternetState::Failed(error) => (error.message(), "Press Back to try again"),
            InternetState::Networks(_) => ("No networks found", "Press Back to try again"),
            _ => ("Searching…", "Looking for Wi-Fi networks"),
        }
    }

    fn open_networks(&mut self) {
        self.networks.clear();
        self.send_internet(InternetCommand::Scan);
        self.enter(Page::WifiNetworks);
    }

    fn return_to_internet(&mut self) {
        self.keyboard = None;
        self.page = Page::Internet;
        self.focus = 0;
        self.scroll = 0;
        self.editing = false;
        self.redraw();
    }

    pub(super) fn activate_internet(&mut self, item: Item) {
        match item {
            Item::Internet => self.enter(Page::Internet),
            Item::InternetNetwork => self.open_networks(),
            Item::InternetTest => {
                self.awaiting_connect = true;
                self.send_internet(InternetCommand::Connect);
                self.redraw();
            }
            Item::Network(index) => self.choose_network(index),
            Item::NetworkForget => {
                for saved in saved_networks::load(&self.state_dir) {
                    if let Err(error) = saved_networks::forget(&self.state_dir, &saved.ssid) {
                        tracing::warn!(%error, "could not forget a saved Wi-Fi network");
                        self.error_message = Some("Unable to save settings");
                    }
                }
                self.focus = 0;
                self.redraw();
            }
            _ => {}
        }
    }

    fn choose_network(&mut self, index: usize) {
        let Some(network) = self.networks.get(index).cloned() else {
            return;
        };
        if network.security == Security::Unsupported {
            return;
        }
        let saved = saved_networks::load(&self.state_dir)
            .into_iter()
            .any(|saved| saved.ssid == network.ssid);
        if saved {
            self.send_internet(InternetCommand::JoinSaved { ssid: network.ssid });
            self.return_to_internet();
        } else if network.security == Security::Open {
            self.send_internet(InternetCommand::Join {
                ssid: network.ssid,
                passphrase: None,
            });
            self.return_to_internet();
        } else {
            self.keyboard_ssid = network.ssid.clone();
            self.keyboard = Some(Keyboard::new(network.display_name()));
            self.enter(Page::Keyboard);
        }
    }

    pub(super) fn go_back_internet(&mut self) {
        match self.page {
            Page::Keyboard => {
                self.keyboard = None;
                self.page = Page::WifiNetworks;
                self.focus = 0;
                self.scroll = 0;
                self.redraw();
            }
            Page::WifiNetworks => self.return_to_internet(),
            _ if self.internet_return => {
                self.internet_return = false;
                self.release_internet();
                self.enter(Page::Updates);
            }
            _ => {
                self.release_internet();
                self.leave_subpage();
                self.redraw();
            }
        }
    }

    fn keyboard_action(&mut self, action: KeyboardAction) {
        if let KeyboardAction::Submit(passphrase) = action {
            let ssid = std::mem::take(&mut self.keyboard_ssid);
            self.send_internet(InternetCommand::Join {
                ssid,
                passphrase: Some(passphrase),
            });
            self.return_to_internet();
        } else {
            self.redraw();
        }
    }

    pub(super) fn handle_keyboard_button(&mut self, button: KnobInputButton) {
        match button {
            KnobInputButton::Back => self.go_back_internet(),
            KnobInputButton::Select => {
                let action = self
                    .keyboard
                    .as_mut()
                    .map_or(KeyboardAction::None, Keyboard::select);
                self.keyboard_action(action);
            }
        }
    }

    pub(super) fn handle_keyboard_rotary(&mut self, direction: RotaryDirection, steps: u8) {
        if let Some(keyboard) = self.keyboard.as_mut() {
            keyboard.rotary(direction, steps);
        }
        self.redraw();
    }

    pub(super) fn handle_keyboard_touch(&mut self, phase: TouchInputPhase, x: i32, y: i32) {
        if y < HEADER_HEIGHT {
            if phase == TouchInputPhase::Up {
                self.go_back_internet();
            }
            return;
        }
        let action = self
            .keyboard
            .as_mut()
            .map_or(KeyboardAction::None, |keyboard| keyboard.touch(phase, x, y));
        self.keyboard_action(action);
    }

    pub(super) fn internet_row_view(&self, item: Item) -> RowView {
        match item {
            Item::Internet => match &self.internet_state {
                InternetState::Online { .. } => {
                    RowView::new("Internet", Trailing::Link("Online".to_owned(), GREEN))
                }
                InternetState::Failed(error) => RowView::new(
                    "Internet",
                    Trailing::Link(error.message().to_owned(), SECONDARY),
                ),
                // "Off" read as "Wi-Fi is broken": name the network, with the
                // state on its own line so neither gets cut off.
                _ => match saved_networks::load(&self.state_dir).first() {
                    Some(saved) => RowView::new(
                        "Internet",
                        Trailing::Link(
                            String::from_utf8_lossy(&saved.ssid).into_owned(),
                            SECONDARY,
                        ),
                    )
                    .detail("Not connected"),
                    None => RowView::new(
                        "Internet",
                        Trailing::Link("Not connected".to_owned(), SECONDARY),
                    ),
                },
            },
            Item::InternetNetwork => {
                let summary = saved_networks::load(&self.state_dir).first().map_or_else(
                    || "Choose…".to_owned(),
                    |saved| String::from_utf8_lossy(&saved.ssid).into_owned(),
                );
                RowView::new("Network", Trailing::Link(summary, SECONDARY))
            }
            Item::InternetTest => RowView::new("Test connection", Trailing::None),
            Item::Network(index) => {
                let Some(network) = self.networks.get(index) else {
                    return RowView::new("", Trailing::None);
                };
                let detail = match network.security {
                    Security::Open => "Open",
                    Security::Psk => "Secured",
                    Security::Unsupported => "Not supported",
                };
                let view = RowView::new(
                    network.display_name(),
                    Trailing::Value(signal_word(network.signal_dbm).to_owned(), SECONDARY),
                )
                .detail(detail);
                if network.security == Security::Unsupported {
                    view.color(SECONDARY)
                } else {
                    view.color(WHITE)
                }
            }
            Item::NetworkForget => RowView::new("Forget saved networks", Trailing::None).color(RED),
            _ => RowView::new("", Trailing::None),
        }
    }
}

const fn signal_word(signal_dbm: i32) -> &'static str {
    if signal_dbm >= -60 {
        "Strong"
    } else if signal_dbm >= -75 {
        "Good"
    } else {
        "Weak"
    }
}
