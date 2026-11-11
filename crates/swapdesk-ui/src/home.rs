//! The home screen "Início" (D35): this computer's name and state, the network chip and a list of cards. The view is
//! built here once so the Windows painter, the macOS view model and the accessibility tree read the same texts, in
//! the same order. A later feature is one more entry in [`HomeView::cards`].

use crate::api::Msg;
use crate::api::{NetCategoryView, Os, Ready};
use crate::format;
use crate::interact::{ControlId, Specs, UiRole};
use crate::state::{DataKind, EditId, Nav, UiState};
use crate::strings::MsgKey;

/// How many activity lines the "Atividade recente" card shows.
pub const RECENT_LIMIT: usize = 5;

/// One line of the "Atividade recente" card: the same cells the Atividade screen shows for the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentLine {
    pub when: String,
    pub who: String,
    pub what: String,
    pub alert: bool,
    /// What a screen reader says: the Atividade row's label.
    pub label: String,
}

/// The chip at the top right: the network line, and whether it warns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkChip {
    pub text: String,
    /// A public network or a notice about the network: drawn in the warning colours.
    pub warn: bool,
}

/// A card of the home screen, in display order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeCard {
    /// "Parear": the explanation, the button and the add-by-address field.
    Pair { title: String, body: String, button: String, address_label: String, address_hint: String, add: String },
    /// "Atividade recente": the latest entries (or the empty line) and "Ver tudo".
    Recent { title: String, lines: Vec<RecentLine>, empty: String, all: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeView {
    /// This computer's name (`None` until the service answered).
    pub name: Option<String>,
    pub status: Option<String>,
    /// The status dot is the "ok" colour (otherwise the danger one).
    pub ready: bool,
    /// This computer's system, for its icon.
    pub os: Option<Os>,
    pub network: Option<NetworkChip>,
    pub cards: Vec<HomeCard>,
}

impl UiState {
    /// The home screen's content.
    pub fn home(&self) -> HomeView {
        let (name, status, ready) = match self.this_device() {
            Some(t) => (
                Some(t.name.clone()),
                Some(self.t(match t.ready {
                    Ready::ReadyToReceive => MsgKey::ReadyToReceive,
                    Ready::ServiceOff => MsgKey::ReadyServiceOff,
                    Ready::ServiceNeedsApproval => MsgKey::ReadyServiceNeedsApproval,
                    Ready::NoNetwork => MsgKey::ReadyNoNetwork,
                })),
                t.ready == Ready::ReadyToReceive,
            ),
            None => (None, None, true),
        };
        let network = self.network().map(|n| NetworkChip {
            text: match &n.address {
                Some(a) => self.tm(&Msg::new(MsgKey::NetworkLine).with(n.label.clone()).with(a.clone())),
                None => self.tm(&Msg::new(MsgKey::NetworkLineNoAddress).with(n.label.clone())),
            },
            warn: n.category == Some(NetCategoryView::Public) || !n.notices.is_empty(),
        });
        let lines = self
            .activity()
            .iter()
            .take(RECENT_LIMIT)
            .map(|row| {
                let when = format::table_when(self.language(), row.at_ms, self.clock());
                let who = self.activity_who(row);
                let what = self.activity_what(row);
                let label = format!("{when}, {who}, {what}, {}", self.duration_cell(row));
                RecentLine { when, who, what, alert: row.alert, label }
            })
            .collect();
        HomeView {
            name,
            status,
            ready,
            os: self.this_device().map(|t| t.os),
            network,
            cards: vec![
                HomeCard::Pair {
                    title: self.t(MsgKey::HomePairTitle),
                    body: self.t(MsgKey::HomePairBody),
                    button: self.t(MsgKey::PairThisComputer),
                    address_label: self.t(MsgKey::AddressLabel),
                    address_hint: self.t(MsgKey::HomeAddressHint),
                    add: self.t(MsgKey::HomeAddressAdd),
                },
                HomeCard::Recent {
                    title: self.t(MsgKey::HomeRecentTitle),
                    lines,
                    empty: self.t(MsgKey::HomeRecentEmpty),
                    all: self.t(MsgKey::HomeRecentAll),
                },
            ],
        }
    }

    /// The controls of the home screen, in focus order (after the sidebar).
    pub(crate) fn home_controls(&self, s: &mut Specs) {
        let home = self.home();
        if let Some(name) = &home.name {
            s.text(UiRole::Heading, name.clone());
        }
        if let Some(status) = &home.status {
            s.text(UiRole::Text, status.clone());
        }
        if let Some(chip) = &home.network {
            s.text(UiRole::Text, chip.text.clone());
        }
        // The update card comes first on Início (UPDATES.md 10.1).
        if let Some(c) = self.update_card() {
            // A wait (downloading, verifying, installing) is a polite live region, announced when its phase changes;
            // the bar is a progress node outside it, read when focused, never announced each second (R-UI-13).
            let waits = c.icon == crate::update_card::UpdIcon::Wait;
            if waits {
                s.region(crate::interact::Region::Status);
            }
            s.text(UiRole::Heading, c.title);
            for text in c.body.into_iter().chain(c.note) {
                s.text(UiRole::Text, text);
            }
            if waits {
                s.region(crate::interact::Region::Content);
            }
            if let Some(bar) = c.bar {
                let label = bar.text.clone().unwrap_or_default();
                let p = s.add(ControlId::Static(u16::MAX - 1), UiRole::Progress, label);
                p.value = Some(format!("{:.0}", bar.fraction * 100.0));
            }
            for (id, label, _) in c.buttons {
                s.button(id, label);
            }
        }
        self.crash_and_empty_controls(s);
        for card in &home.cards {
            match card {
                HomeCard::Pair { title, body, button, address_label, address_hint, add } => {
                    s.text(UiRole::Heading, title.clone());
                    s.text(UiRole::Text, body.clone());
                    s.button(ControlId::PairThisComputer, button.clone());
                    let c = s.add(ControlId::Edit(EditId::Address), UiRole::TextInput, address_label.clone());
                    c.description = Some(address_hint.clone());
                    s.button(ControlId::AddAddress, add.clone());
                }
                HomeCard::Recent { title, lines, empty, all } => {
                    s.text(UiRole::Heading, title.clone());
                    s.button(ControlId::HomeActivityAll, all.clone());
                    if lines.is_empty() && !self.placeholder(DataKind::Activity) {
                        s.text(UiRole::Text, empty.clone());
                    }
                    for (n, line) in lines.iter().enumerate() {
                        s.add(ControlId::Static(800 + n as u16), UiRole::Row, line.label.clone());
                    }
                }
            }
        }
    }

    /// Whether the home page is the one shown now.
    pub fn is_home(&self) -> bool {
        self.nav() == Nav::Home
    }
}
