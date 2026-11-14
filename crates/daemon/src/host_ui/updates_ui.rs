//! Settings > Version > Updates: check GitHub for a newer Prelude, list
//! every version (for rollback), confirm, and follow the install.

use super::{
    GREEN, Item, OverlaySurface, Page, RowView, SECONDARY, Section, SettingsUi, Trailing, WHITE,
};
use crate::{
    internet::{InternetState, Route},
    saved_networks,
    settings::UpdateChannel,
    updater::{Candidate, Relation, UpdateCommand, UpdateState, system::pending_exists},
};

pub(super) const CONFIRM_TEXT: &str =
    "CarPlay and Android Auto pause during the update. Keep power connected.";
pub(super) const CONFIRM_OLDER_TEXT: &str = "CarPlay and Android Auto pause during the update. Keep power connected. This is older than the installed version.";

impl<D: OverlaySurface> SettingsUi<D> {
    pub(super) const fn in_updates_pages(&self) -> bool {
        matches!(
            self.page,
            Page::Updates | Page::AllVersions | Page::UpdateConfirm | Page::UpdateProgress
        )
    }

    fn send_update(&self, command: UpdateCommand) {
        if let Some(updater) = self.updater.as_ref() {
            updater.send(command);
        }
    }

    /// The Progress page holds the screen while an install is under way.
    pub(super) const fn updates_locked(&self) -> bool {
        matches!(self.page, Page::UpdateProgress)
            && matches!(
                self.update_state,
                UpdateState::Connecting
                    | UpdateState::Downloading { .. }
                    | UpdateState::Verifying
                    | UpdateState::Installing { .. }
            )
    }

    /// Picks up worker state changes; returns whether the screen changed.
    pub(super) fn poll_updates(&mut self) -> bool {
        let Some(state) = self.updater.as_ref().map(crate::updater::Updater::state) else {
            return false;
        };
        if state == self.update_state {
            return false;
        }
        if let UpdateState::Versions(list) = &state {
            self.update_versions.clone_from(list);
        }
        self.update_state = state;
        self.scroll = self.scroll.min(self.layout().maximum_scroll());
        self.focus = self.focus.min(self.layout().rows.len().saturating_sub(1));
        self.in_updates_pages()
    }

    fn channel_name(&self) -> &'static str {
        match self.settings.update_channel {
            UpdateChannel::Stable => "Stable",
            UpdateChannel::Beta => "Beta",
        }
    }

    fn restart_pending(&self) -> bool {
        matches!(self.update_state, UpdateState::Installed { .. })
            || pending_exists(&self.state_dir)
    }

    pub(super) fn update_caption(&self) -> Option<String> {
        Some(match &self.update_state {
            UpdateState::Unavailable(error) | UpdateState::Failed(error) => {
                error.message().to_owned()
            }
            UpdateState::Connecting => "Connecting…".to_owned(),
            UpdateState::Checking => "Checking…".to_owned(),
            UpdateState::UpToDate => "Up to date".to_owned(),
            UpdateState::Available(candidate) => format!("{} available", candidate.version),
            UpdateState::Downloading { done, total, .. } => {
                let percent = if *total == 0 {
                    0
                } else {
                    done.saturating_mul(100) / total
                };
                format!("Downloading {}%", percent.min(100))
            }
            UpdateState::Verifying => "Verifying…".to_owned(),
            UpdateState::Installing { .. } => "Installing… keep power connected".to_owned(),
            UpdateState::Installed { .. } => "Installed - restart to finish".to_owned(),
            UpdateState::Idle | UpdateState::Versions(_) if self.restart_pending() => {
                "Installed - restart to finish".to_owned()
            }
            UpdateState::Idle | UpdateState::Versions(_) => return None,
        })
    }

    pub(super) fn updates_sections(&self) -> Vec<Section> {
        match self.page {
            Page::Updates => {
                if matches!(self.update_state, UpdateState::Unavailable(_)) {
                    return vec![Section {
                        title: self.update_caption(),
                        ..Section::plain(vec![Item::UpdateInstalled])
                    }];
                }
                let mut items = vec![Item::UpdateInstalled, Item::UpdateChannel];
                if self.internet.is_some() {
                    items.push(Item::UpdateNetwork);
                }
                items.extend([Item::CheckUpdates, Item::ShowAllVersions]);
                if matches!(self.update_state, UpdateState::Available(_)) {
                    items.push(Item::InstallUpdate);
                }
                if self.restart_pending() {
                    items.push(Item::RestartNow);
                }
                vec![Section {
                    title: self.update_caption(),
                    ..Section::plain(items)
                }]
            }
            Page::AllVersions if !self.update_versions.is_empty() => vec![Section::plain(
                (0..self.update_versions.len())
                    .map(Item::UpdateVersion)
                    .collect(),
            )],
            Page::UpdateConfirm => {
                let older = self
                    .update_choice
                    .as_ref()
                    .is_some_and(|choice| choice.relation == Relation::Older);
                vec![
                    Section::plain(vec![Item::ConfirmInstall, Item::CancelUpdate]).footer(
                        if older {
                            CONFIRM_OLDER_TEXT
                        } else {
                            CONFIRM_TEXT
                        },
                    ),
                ]
            }
            Page::UpdateProgress => {
                let items = match self.update_state {
                    UpdateState::Connecting | UpdateState::Downloading { .. } => {
                        vec![Item::CancelUpdate]
                    }
                    UpdateState::Installed { .. } => vec![Item::RestartNow, Item::Later],
                    UpdateState::Failed(_) | UpdateState::Idle => vec![Item::Later],
                    _ => Vec::new(),
                };
                vec![Section {
                    title: self.update_caption(),
                    ..Section::plain(items)
                }]
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn all_versions_empty_text(&self) -> (&'static str, &'static str) {
        match &self.update_state {
            UpdateState::Failed(error) => (error.message(), "Press Back to try again"),
            UpdateState::Versions(_) => ("No versions found", "Press Back to try again"),
            _ => ("Checking…", "Looking for Prelude releases"),
        }
    }

    pub(super) fn open_updates(&mut self) {
        if self.updater.is_some() {
            self.enter(Page::Updates);
        }
    }

    fn confirm(&mut self, candidate: Candidate) {
        self.update_choice = Some(candidate);
        self.enter(Page::UpdateConfirm);
    }

    pub(super) fn activate_updates(&mut self, item: Item) {
        match item {
            Item::UpdateChannel => {
                let mut next = self.settings;
                next.update_channel = match next.update_channel {
                    UpdateChannel::Stable => UpdateChannel::Beta,
                    UpdateChannel::Beta => UpdateChannel::Stable,
                };
                self.apply_immediate_setting(next, false);
            }
            Item::UpdateNetwork => {
                self.internet_return = true;
                self.enter(Page::Internet);
            }
            Item::CheckUpdates => {
                self.send_update(UpdateCommand::Check {
                    channel: self.settings.update_channel,
                });
                self.redraw();
            }
            Item::ShowAllVersions => {
                self.update_versions.clear();
                self.send_update(UpdateCommand::ListAll {
                    channel: self.settings.update_channel,
                });
                self.enter(Page::AllVersions);
            }
            Item::InstallUpdate => {
                if let UpdateState::Available(candidate) = &self.update_state {
                    self.confirm(candidate.clone());
                }
            }
            Item::UpdateVersion(index) => {
                if let Some(candidate) = self.update_versions.get(index).cloned()
                    && candidate.relation != Relation::Installed
                {
                    self.confirm(candidate);
                }
            }
            Item::ConfirmInstall => {
                if let Some(candidate) = self.update_choice.clone() {
                    self.send_update(UpdateCommand::Install(candidate));
                    self.enter(Page::UpdateProgress);
                }
            }
            Item::CancelUpdate if self.page == Page::UpdateProgress => {
                self.send_update(UpdateCommand::Cancel);
            }
            Item::CancelUpdate => self.enter(Page::Updates),
            Item::RestartNow => self.send_update(UpdateCommand::Restart),
            Item::Later => {
                self.send_update(UpdateCommand::Dismiss);
                self.enter(Page::Updates);
            }
            _ => {}
        }
    }

    pub(super) fn go_back_updates(&mut self) {
        match self.page {
            Page::UpdateProgress => match self.update_state {
                UpdateState::Connecting | UpdateState::Downloading { .. } => {
                    self.send_update(UpdateCommand::Cancel);
                }
                UpdateState::Verifying | UpdateState::Installing { .. } => {}
                _ => self.enter(Page::Updates),
            },
            Page::AllVersions | Page::UpdateConfirm => self.enter(Page::Updates),
            _ => {
                self.leave_subpage();
                self.redraw();
            }
        }
    }

    pub(super) fn updates_row_view(&self, item: Item) -> RowView {
        match item {
            Item::UpdateInstalled => RowView::new(
                "Installed",
                // The channel has its own row; both together do not fit.
                Trailing::Value(self.version.clone(), SECONDARY),
            ),
            Item::UpdateChannel => RowView::new(
                "Channel",
                Trailing::Value(self.channel_name().to_owned(), SECONDARY),
            ),
            Item::UpdateNetwork => {
                let summary = match &self.internet_state {
                    InternetState::Online {
                        route: Route::Wired { .. },
                        ..
                    } => "Wired".to_owned(),
                    _ => saved_networks::load(&self.state_dir).first().map_or_else(
                        || "Choose…".to_owned(),
                        |saved| String::from_utf8_lossy(&saved.ssid).into_owned(),
                    ),
                };
                RowView::new("Network", Trailing::Link(summary, SECONDARY))
            }
            Item::CheckUpdates => RowView::new("Check for updates", Trailing::None),
            Item::ShowAllVersions => RowView::new(
                "Show all versions",
                Trailing::Link(String::new(), SECONDARY),
            ),
            Item::InstallUpdate => match &self.update_state {
                UpdateState::Available(candidate) => {
                    RowView::new(format!("Install {}", candidate.version), Trailing::None)
                        .color(GREEN)
                }
                _ => RowView::new("", Trailing::None),
            },
            Item::UpdateVersion(index) => {
                let Some(candidate) = self.update_versions.get(index) else {
                    return RowView::new("", Trailing::None);
                };
                let detail = match candidate.relation {
                    Relation::Newer => "Newer",
                    Relation::Installed => "Installed",
                    Relation::Older => "Older - rollback",
                };
                let view =
                    RowView::new(candidate.version.to_string(), Trailing::None).detail(detail);
                if candidate.relation == Relation::Installed {
                    view.color(SECONDARY)
                } else {
                    view.color(WHITE)
                }
            }
            Item::ConfirmInstall => RowView::new(
                self.update_choice
                    .as_ref()
                    .map_or_else(String::new, |choice| format!("Install {}", choice.version)),
                Trailing::None,
            )
            .color(GREEN),
            Item::CancelUpdate => RowView::new("Cancel", Trailing::None),
            Item::RestartNow => RowView::new("Restart now", Trailing::None).color(GREEN),
            Item::Later => RowView::new("Later", Trailing::None),
            _ => RowView::new("", Trailing::None),
        }
    }
}
