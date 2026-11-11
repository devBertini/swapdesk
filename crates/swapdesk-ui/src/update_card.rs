//! The update card of Início and the lines of the Ajustes section "Atualizações" (UPDATES.md 10). Built here once so
//! the painter of each system and the accessibility tree read the same texts and buttons, in the same order.

use crate::api::{CheckFailure, Msg, UpdateFailure, UpdateInfo, UpdatePhase};
use crate::format;
use crate::interact::ControlId;
use crate::state::{UiState, WaitBar};
use crate::strings::MsgKey;

/// The release page of a version (UPDATES.md 2.1: the repository's releases).
pub fn release_page(version: &str) -> String {
    format!("https://github.com/devBertini/swapdesk/releases/tag/v{version}")
}

/// What a button of the update card or section does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpdAction {
    /// "Atualizar agora".
    Install,
    /// "Agora não": hides the card for this version.
    Later,
    /// "O que mudou" and "Abrir a página da versão": the release page in the browser.
    Notes,
    /// "Encerrar e atualizar" in the live-session confirmation.
    Confirm,
    /// "Cancelar" in the confirmation.
    CancelConfirm,
    /// "Cancelar" while downloading (the partial file is deleted).
    CancelDownload,
    /// "Tentar de novo": a new check, which offers the version again.
    Retry,
    /// "Fechar" on the card after an update, or after a rollback.
    Close,
    /// "Baixar à mão": the release page.
    ByHand,
    /// Ajustes: "Procurar agora".
    CheckNow,
    /// Ajustes: the switch "Procurar novas versões no GitHub".
    Switch,
}

/// The card's icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdIcon {
    /// A version on offer.
    New,
    /// Waiting (downloading, verifying, installing).
    Wait,
    /// Updated.
    Done,
    /// A failure or a rollback.
    Warn,
}

/// The update card.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateCard {
    pub icon: UpdIcon,
    pub title: String,
    pub body: Option<String>,
    /// The second paragraph of the live-session confirmation.
    pub note: Option<String>,
    pub bar: Option<WaitBar>,
    /// In display order, negative left, positive right; `true` marks the primary one.
    pub buttons: Vec<(ControlId, String, bool)>,
}

impl UiState {
    /// The card of the update, when there is one to show (10.1).
    pub fn update_card(&self) -> Option<UpdateCard> {
        let u = self.update.as_ref()?;
        let b = |a: UpdAction, key: MsgKey, primary: bool| (ControlId::Update(a), self.t(key), primary);
        let card = |icon, title: String, body: Option<String>, buttons| UpdateCard {
            icon,
            title,
            body,
            note: None,
            bar: None,
            buttons,
        };
        let title = |v: &str| self.tm(&Msg::new(MsgKey::UpdTitle).with(v));
        Some(match &u.phase {
            UpdatePhase::Downloading { version, done, total } => {
                let mut c = card(
                    UpdIcon::Wait,
                    title(version),
                    Some(
                        self.tm(&Msg::new(MsgKey::UpdDownloading)
                            .with(format::size_mb(self.lang, *done))
                            .with(format::size_mb(self.lang, *total))),
                    ),
                    vec![b(UpdAction::CancelDownload, MsgKey::UpdCancel, false)],
                );
                let fraction = if *total == 0 { 0.0 } else { (*done as f32 / *total as f32).clamp(0.0, 1.0) };
                // The time left from the rate measured over the last seconds (R-UI-13); none until there is one.
                let left = (self.update_rate > 0).then(|| {
                    let secs = total.saturating_sub(*done).div_ceil(self.update_rate);
                    format::countdown(u16::try_from(secs).unwrap_or(u16::MAX))
                });
                c.bar = Some(WaitBar { fraction, text: left });
                c
            }
            UpdatePhase::Verifying { version } => {
                card(UpdIcon::Wait, title(version), Some(self.t(MsgKey::UpdVerifying)), Vec::new())
            }
            UpdatePhase::Installing { version } => {
                card(UpdIcon::Wait, title(version), Some(self.t(MsgKey::UpdInstalling)), Vec::new())
            }
            UpdatePhase::Failed(f) => {
                let key = match f {
                    UpdateFailure::Download => MsgKey::UpdErrNetwork,
                    UpdateFailure::Mismatch => MsgKey::UpdErrMismatch,
                    UpdateFailure::Disk => MsgKey::UpdErrDisk,
                    UpdateFailure::Unwritable => MsgKey::UpdErrMacAdmin,
                    UpdateFailure::Translocated => MsgKey::UpdErrMacLocation,
                    // Refusals and a cancel are answered where they were asked: no card.
                    _ => return self.idle_card(u),
                };
                let text = if key == MsgKey::UpdErrDisk {
                    self.tm(&Msg::new(key).with(format::size_mb(self.lang, self.update_size)))
                } else {
                    self.t(key)
                };
                // Only an administrator can replace the app: the release page, to install it by hand.
                let way_out = if *f == UpdateFailure::Unwritable {
                    b(UpdAction::Notes, MsgKey::UpdOpenRelease, true)
                } else {
                    b(UpdAction::Retry, MsgKey::UpdRetry, true)
                };
                card(UpdIcon::Warn, text, None, vec![way_out])
            }
            _ => return self.idle_card(u),
        })
    }

    /// The card when nothing runs: after an update, after a rollback, or a version on offer.
    fn idle_card(&self, u: &UpdateInfo) -> Option<UpdateCard> {
        let b = |a: UpdAction, key: MsgKey, primary: bool| (ControlId::Update(a), self.t(key), primary);
        if u.updated {
            return Some(UpdateCard {
                icon: UpdIcon::Done,
                title: self.tm(&Msg::new(MsgKey::UpdDone).with(&u.running)),
                body: None,
                note: None,
                bar: None,
                buttons: vec![
                    b(UpdAction::Close, MsgKey::UpdClose, false),
                    b(UpdAction::Notes, MsgKey::UpdNotes, true),
                ],
            });
        }
        if let Some(v) = &u.rolled_back {
            return Some(UpdateCard {
                icon: UpdIcon::Warn,
                title: self.tm(&Msg::new(MsgKey::UpdErrRolledBack).with(v).with(&u.running)),
                body: None,
                note: None,
                bar: None,
                buttons: vec![
                    b(UpdAction::Close, MsgKey::UpdClose, false),
                    b(UpdAction::ByHand, MsgKey::UpdDownloadByHand, false),
                    b(UpdAction::Retry, MsgKey::UpdRetry, true),
                ],
            });
        }
        let UpdatePhase::Offered { version, manual } = &u.phase else { return None };
        if u.dismissed.as_deref() == Some(version.as_str()) && !self.update_confirm {
            return None;
        }
        let title = self.tm(&Msg::new(MsgKey::UpdTitle).with(version));
        if *manual {
            return Some(UpdateCard {
                icon: UpdIcon::New,
                title,
                body: Some(self.t(MsgKey::UpdManual)),
                note: None,
                bar: None,
                buttons: vec![
                    b(UpdAction::Later, MsgKey::UpdLater, false),
                    b(UpdAction::Notes, MsgKey::UpdOpenRelease, true),
                ],
            });
        }
        if self.update_confirm {
            let peer = self.live_peer_name().unwrap_or_default();
            return Some(UpdateCard {
                icon: UpdIcon::Warn,
                title,
                body: Some(self.tm(&Msg::new(MsgKey::UpdLiveBody).with(peer))),
                note: self.hosting_now().then(|| self.t(MsgKey::UpdLiveRemote)),
                bar: None,
                buttons: vec![
                    b(UpdAction::CancelConfirm, MsgKey::UpdCancel, false),
                    b(UpdAction::Confirm, MsgKey::UpdLiveConfirm, true),
                ],
            });
        }
        Some(UpdateCard {
            icon: UpdIcon::New,
            title,
            body: Some(self.tm(&Msg::new(MsgKey::UpdBody).with(&u.running))),
            note: None,
            bar: None,
            buttons: vec![
                b(UpdAction::Later, MsgKey::UpdLater, false),
                b(UpdAction::Notes, MsgKey::UpdNotes, false),
                b(UpdAction::Install, MsgKey::UpdInstall, true),
            ],
        })
    }

    /// The Ajustes lines of the section "Atualizações": the version, the last check (or why it failed), and whether
    /// this is the latest version. `None` when the service has no updater.
    pub fn update_lines(&self) -> Option<Vec<String>> {
        let u = self.update.as_ref()?;
        let mut lines = vec![self.tm(&Msg::new(MsgKey::UpdVersion).with(&u.running))];
        match u.last_failure {
            Some(CheckFailure::Network) => lines.push(self.t(MsgKey::UpdErrNetwork)),
            Some(CheckFailure::RateLimited) => lines.push(self.t(MsgKey::UpdErrWait)),
            Some(CheckFailure::Signature) => lines.push(self.t(MsgKey::UpdErrSignature)),
            None => {
                if let Some(at) = u.last_check_ms {
                    let when = format::when_phrase(self.lang, at, self.clock());
                    lines.push(self.tm(&Msg::new(MsgKey::UpdLast).with(when)));
                }
                if matches!(u.phase, UpdatePhase::Idle) && u.last_check_ms.is_some() {
                    lines.push(self.t(MsgKey::UpdLatest));
                }
            }
        }
        Some(lines)
    }

    /// The updater's view, when the service has one.
    pub fn update_info(&self) -> Option<&UpdateInfo> {
        self.update.as_ref()
    }

    /// Whether "Procurar agora" can run (not while a check or an install runs).
    pub fn update_check_enabled(&self) -> bool {
        self.update.as_ref().is_some_and(|u| {
            !matches!(
                u.phase,
                UpdatePhase::Checking
                    | UpdatePhase::Downloading { .. }
                    | UpdatePhase::Verifying { .. }
                    | UpdatePhase::Installing { .. }
            )
        })
    }

    /// The name of the peer of a live session, for the confirmation.
    fn live_peer_name(&self) -> Option<String> {
        let s = self.sessions.iter().find(|s| matches!(s.state, crate::api::SessionState::Active { .. }))?;
        Some(self.device_by_id(&s.device_id).map_or_else(|| s.device_id.clone(), |d| d.name.clone()))
    }

    /// This computer is being controlled: the confirmation adds the line for the person who may be clicking from
    /// elsewhere (10.3).
    fn hosting_now(&self) -> bool {
        self.host_session().is_some()
    }

    /// What a button of the update card or section does (UPDATES.md 10.2).
    pub(crate) fn activate_update(&mut self, action: UpdAction, a: &mut crate::state::Activation) {
        use crate::api::Command as C;
        let Some(u) = self.update.clone() else { return };
        let offered = match &u.phase {
            UpdatePhase::Offered { version, .. } => Some(version.clone()),
            _ => None,
        };
        match action {
            UpdAction::Install => {
                if let Some(version) = offered {
                    a.commands.push(C::UpdateInstall { version, end_sessions: false });
                }
            }
            UpdAction::Confirm => {
                self.update_confirm = false;
                if let Some(version) = offered {
                    a.commands.push(C::UpdateInstall { version, end_sessions: true });
                }
            }
            UpdAction::CancelConfirm => self.update_confirm = false,
            UpdAction::Later => {
                if let Some(version) = offered {
                    // Hidden at once; the service keeps it for the next start.
                    if let Some(info) = &mut self.update {
                        info.dismissed = Some(version.clone());
                    }
                    a.commands.push(C::UpdateDismiss { version });
                }
            }
            UpdAction::Notes => {
                // The version on offer, the one just installed, or else the latest release.
                self.web_page = Some(match offered {
                    Some(v) => release_page(&v),
                    None if u.updated => release_page(&u.running),
                    None => "https://github.com/devBertini/swapdesk/releases/latest".into(),
                });
                a.effect = Some(crate::state::Effect::OpenWebPage);
            }
            UpdAction::ByHand => {
                let version = u.rolled_back.clone().unwrap_or_else(|| u.running.clone());
                self.web_page = Some(release_page(&version));
                a.effect = Some(crate::state::Effect::OpenWebPage);
            }
            UpdAction::CancelDownload => a.commands.push(C::UpdateCancel),
            UpdAction::Retry | UpdAction::CheckNow => a.commands.push(C::UpdateCheckNow),
            UpdAction::Close => {
                // The "updated" card closes by the running version, the rollback card by the version rolled back.
                let version = if u.updated { Some(u.running.clone()) } else { u.rolled_back.clone() };
                if let Some(info) = &mut self.update {
                    if info.updated {
                        info.updated = false;
                    } else {
                        info.rolled_back = None;
                    }
                }
                if let Some(version) = version {
                    a.commands.push(C::UpdateDismiss { version });
                }
            }
            UpdAction::Switch => {
                let on = !u.check_on;
                if let Some(info) = &mut self.update {
                    info.check_on = on;
                }
                a.commands.push(C::UpdateSetCheck { on });
            }
        }
        a.changed = crate::state::Changed::ALL;
    }
}
