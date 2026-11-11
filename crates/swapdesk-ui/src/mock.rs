//! `MockBackend` (C7): scripted replies and events for the shells, with one named scenario per approved screen state
//! and per state the design lacks. Development builds of `Swapdesk.exe` pick one with `--mock <scenario>`.
//!
//! A scenario is a script of steps replayed on a fresh [`UiState`]: events as the service would send them and the
//! user's own moves (select a device, open a tab, toggle a switch). The same script drives the offscreen captures and
//! the tests, so every screen the shell draws is one the state can really reach.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::api::*;
use crate::format::Clock;
use crate::inflight::{InFlight, Joined};
use crate::interact::ControlId;
use crate::state::{DataKind, DeviceTab, DrawerTab, Nav, UiState};
use crate::strings::MsgKey;

/// 2026-11-08 15:30 UTC: the fixed "now" of every scenario, so captures do not change from day to day.
pub const NOW_MS: u64 = 1_794_151_800_000;

const MIN: u64 = 60_000;
const HOUR: u64 = 60 * MIN;
const DAY: u64 = 24 * HOUR;

/// A moment `back_days` days before today at `hour:minute` (UTC, the scenario clock has no offset).
fn at(hour: u64, minute: u64, back_days: u64) -> u64 {
    NOW_MS - (15 * HOUR + 30 * MIN) + hour * HOUR + minute * MIN - back_days * DAY
}

pub const MAC: &str = "mac-alex";

/// Bit 63 of an id marks a connect entry (or a wake's), as the service does.
const ENTRY_BIT: u64 = 1 << 63;

/// The id of a connect entry in the scenarios.
pub const ENTRY: u64 = ENTRY_BIT | 7;
pub const DESKTOP: &str = "desktop-escritorio";
pub const AIR: &str = "mac-air";

fn perms(list: &[Permission]) -> Permissions {
    list.iter().fold(Permissions::NONE, |p, x| p.with(*x))
}

fn devices() -> Vec<DeviceView> {
    vec![
        DeviceView {
            id: MAC.into(),
            name: "MacBook do escritório".into(),
            os: Os::MacOs,
            os_version: "15".into(),
            status: DeviceStatus::Online { address: "192.168.0.21".into(), rtt_ms: 3 },
            paired: true,
            grants_here: perms(&[
                Permission::View,
                Permission::Control,
                Permission::Audio,
                Permission::Terminal,
                Permission::Files,
                Permission::Clipboard,
                Permission::Unattended,
            ]),
            grants_there: Some(perms(&[
                Permission::View,
                Permission::Control,
                Permission::Audio,
                Permission::Files,
                Permission::Clipboard,
            ])),
            last_session: Some(SessionSummary {
                at_ms: at(23, 40, 1),
                duration_s: 360,
                direction: Direction::Incoming,
                desktop: DesktopKind::LoginWindow,
            }),
            wakeable: false,
        },
        DeviceView {
            id: DESKTOP.into(),
            name: "Desktop do escritório".into(),
            os: Os::Windows,
            os_version: "11".into(),
            status: DeviceStatus::Asleep,
            paired: true,
            grants_here: perms(&[Permission::View, Permission::Control]),
            grants_there: Some(perms(&[Permission::View, Permission::Control, Permission::Files])),
            last_session: None,
            wakeable: true,
        },
        DeviceView {
            id: AIR.into(),
            name: "MacBook Air".into(),
            os: Os::MacOs,
            os_version: "14".into(),
            status: DeviceStatus::OnNetworkUnpaired { pairing_open: false },
            paired: false,
            grants_here: Permissions::NONE,
            grants_there: None,
            last_session: None,
            wakeable: false,
        },
    ]
}

fn this_device() -> ThisDevice {
    ThisDevice { name: "Notebook da sala".into(), os: Os::Windows, ready: Ready::ReadyToReceive, legacy_note: false }
}

fn network(notices: Vec<Notice>, category: NetCategoryView) -> NetworkStatus {
    NetworkStatus {
        label: "Wi-Fi Casa".into(),
        address: Some("192.168.0.12".into()),
        category: Some(category),
        notices,
    }
}

fn settings() -> Settings {
    Settings {
        name: "Notebook da sala".into(),
        language: Language::PtBr,
        open_at_login: true,
        service_enabled: true,
        lock_on_end: true,
        keep_awake: true,
        quality: Quality::Auto,
        show_stats: false,
        keyboard_mode: KeyboardMode::Auto,
        swap_ctrl_cmd: true,
        theme: Theme::System,
    }
}

fn audit(at_ms: u64, device: &str, what: Msg, duration_s: Option<u32>, alert: bool) -> AuditView {
    AuditView { at_ms, device_name: device.into(), what, duration_s, alert }
}

fn activity() -> Vec<AuditView> {
    let mac = "MacBook do escritório";
    vec![
        audit(at(15, 25, 0), mac, Msg::new(MsgKey::AuditSwap).with("MacBook"), Some(0), false),
        audit(at(15, 8, 0), mac, Msg::new(MsgKey::AuditControlFiles).with("3"), Some(17 * 60), false),
        audit(at(14, 2, 0), "", Msg::new(MsgKey::AuditUnknownPair).with("192.168.0.57"), None, true),
        audit(at(23, 40, 1), mac, Msg::new(MsgKey::AuditUnattendedLogin), Some(6 * 60), false),
        audit(at(19, 12, 1), mac, Msg::new(MsgKey::AuditViewOnly), Some(4 * 60), false),
        audit(at(10, 30, 3), mac, Msg::new(MsgKey::AuditPaired), None, false),
    ]
}

fn reply(result: Result<Reply, ApiError>) -> UiEvent {
    UiEvent::Reply { ticket: Ticket(0), result }
}

/// A step of a scenario script.
#[derive(Debug, Clone)]
pub enum Step {
    Event(UiEvent),
    Select(&'static str),
    Tab(DeviceTab),
    Goto(Nav),
    Activate(ControlId),
    /// Activate a control and answer its command with an error, as the service would.
    ActivateRefused(ControlId, ApiCode),
    /// The launch requests (R-UI-6), sent and not yet answered.
    Startup,
    /// A request for `DataKind` went out and has not been answered.
    BeginFetch(DataKind),
    /// 100 ms passed without answers: placeholders show (R-UI-1).
    ArmPlaceholders,
    /// The last run crashed (D12).
    CrashNotice,
    /// `n` seconds pass.
    Ticks(u16),
}

/// One named state of the interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// "Início" (D35): the home screen.
    Inicio,
    InicioEscuro,
    Dispositivos,
    DispositivosEscuro,
    Permissoes,
    Historico,
    Pareamento,
    ParearEsteComputador,
    PedidoConexao,
    Consentimento,
    Elevacao,
    Atividade,
    Ajustes,
    EstadoRedePublica,
    EstadoServicoMac,
    EstadoPermissoesMac,
    EstadoReconectando,
    EstadoAcordando,
    /// "Controlar" pressed: the card before the other computer has said anything.
    EstadoConectando,
    /// The other computer shows the request: the card with the seconds left.
    EstadoAguardandoAceite,
    /// The other computer's user said no.
    EstadoConexaoRecusada,
    /// Nobody is at the other computer (the longest line).
    EstadoConexaoNinguem,
    EstadoInversaoRecusada,
    Visualizador,
    VisualizadorEscuro,
    Terminal,
    Arquivos,
    Chat,
    /// The drawer's "Área de transferência" tab (1.1: the Mac viewer has it too).
    AreaTransferencia,
    Banner,
    Inverter,
    InverterPedido,
    /// The viewer asked to swap and waits for the other side, with the seconds left (R-UI-13).
    InverterAguardando,
    /// This side confirmed the pairing code and waits for the other side (R-UI-13).
    PareamentoConfirmando,
    /// The last run ended in a panic (D12).
    EstadoFechouInesperado,
    /// The device list is empty after 10 s of discovery (F20).
    EstadoNenhumComputador,
    /// A computer of the network that is not paired yet: its page holds one card (D51).
    EstadoNaoPareado,
    /// Atividade before anything was logged: the empty page (D51).
    AtividadeVazia,
    /// Launch before any answer: placeholder blocks (R-UI-1).
    Carregando,
    AtividadeCarregando,
    ArquivosCarregando,
    /// The terminal panel while the host asks its user (D13), with the seconds left (R-UI-13).
    TerminalAguardandoConsentimento,
    /// Permissões on a Mac: the administrator terminal is off and inert, with its line (ELEVATION-MAC.md M5).
    PermissoesNoMac,
    /// The files panel while the host asks its user (D13), with the seconds left (R-UI-13).
    ArquivosAguardandoConsentimento,
    /// An account with no pairing on a computer a 1.0 install left data on: the one-time note under Pareados (D55).
    PareadosAntesDa11,
}

impl Scenario {
    pub const ALL: [Scenario; 45] = [
        Scenario::Inicio,
        Scenario::InicioEscuro,
        Scenario::Dispositivos,
        Scenario::DispositivosEscuro,
        Scenario::Permissoes,
        Scenario::Historico,
        Scenario::Pareamento,
        Scenario::ParearEsteComputador,
        Scenario::PedidoConexao,
        Scenario::Consentimento,
        Scenario::Elevacao,
        Scenario::Atividade,
        Scenario::Ajustes,
        Scenario::EstadoRedePublica,
        Scenario::EstadoServicoMac,
        Scenario::EstadoPermissoesMac,
        Scenario::EstadoReconectando,
        Scenario::EstadoAcordando,
        Scenario::EstadoConectando,
        Scenario::EstadoAguardandoAceite,
        Scenario::EstadoConexaoRecusada,
        Scenario::EstadoConexaoNinguem,
        Scenario::EstadoInversaoRecusada,
        Scenario::Visualizador,
        Scenario::VisualizadorEscuro,
        Scenario::Terminal,
        Scenario::Arquivos,
        Scenario::Chat,
        Scenario::AreaTransferencia,
        Scenario::Banner,
        Scenario::Inverter,
        Scenario::InverterPedido,
        Scenario::InverterAguardando,
        Scenario::PareamentoConfirmando,
        Scenario::EstadoFechouInesperado,
        Scenario::EstadoNenhumComputador,
        Scenario::EstadoNaoPareado,
        Scenario::AtividadeVazia,
        Scenario::Carregando,
        Scenario::AtividadeCarregando,
        Scenario::ArquivosCarregando,
        Scenario::TerminalAguardandoConsentimento,
        Scenario::ArquivosAguardandoConsentimento,
        Scenario::PermissoesNoMac,
        Scenario::PareadosAntesDa11,
    ];

    /// The name `--mock` takes.
    pub const fn name(self) -> &'static str {
        match self {
            Scenario::Inicio => "inicio",
            Scenario::InicioEscuro => "inicio-escuro",
            Scenario::Dispositivos => "dispositivos",
            Scenario::DispositivosEscuro => "dispositivos-escuro",
            Scenario::Permissoes => "permissoes",
            Scenario::Historico => "historico",
            Scenario::Pareamento => "pareamento",
            Scenario::ParearEsteComputador => "parear-este-computador",
            Scenario::PedidoConexao => "pedido-conexao",
            Scenario::Consentimento => "consentimento",
            Scenario::Elevacao => "elevacao",
            Scenario::Atividade => "atividade",
            Scenario::Ajustes => "ajustes",
            Scenario::EstadoRedePublica => "estado-rede-publica",
            Scenario::EstadoServicoMac => "estado-servico-mac",
            Scenario::EstadoPermissoesMac => "estado-permissoes-mac",
            Scenario::EstadoReconectando => "estado-reconectando",
            Scenario::EstadoAcordando => "estado-acordando",
            Scenario::EstadoConectando => "estado-conectando",
            Scenario::EstadoAguardandoAceite => "estado-aguardando-aceite",
            Scenario::EstadoConexaoRecusada => "estado-conexao-recusada",
            Scenario::EstadoConexaoNinguem => "estado-conexao-ninguem",
            Scenario::EstadoInversaoRecusada => "estado-inversao-recusada",
            Scenario::Visualizador => "visualizador",
            Scenario::VisualizadorEscuro => "visualizador-escuro",
            Scenario::Terminal => "terminal",
            Scenario::Arquivos => "arquivos",
            Scenario::Chat => "chat",
            Scenario::AreaTransferencia => "area-de-transferencia",
            Scenario::Banner => "banner",
            Scenario::Inverter => "inverter",
            Scenario::InverterPedido => "inverter-pedido",
            Scenario::InverterAguardando => "inverter-aguardando",
            Scenario::PareamentoConfirmando => "pareamento-confirmando",
            Scenario::EstadoFechouInesperado => "estado-fechou-inesperado",
            Scenario::EstadoNenhumComputador => "estado-nenhum-computador",
            Scenario::EstadoNaoPareado => "estado-nao-pareado",
            Scenario::PareadosAntesDa11 => "pareados-antes-da-1-1",
            Scenario::AtividadeVazia => "atividade-vazia",
            Scenario::Carregando => "carregando",
            Scenario::AtividadeCarregando => "atividade-carregando",
            Scenario::ArquivosCarregando => "arquivos-carregando",
            Scenario::TerminalAguardandoConsentimento => "terminal-aguardando-consentimento",
            Scenario::ArquivosAguardandoConsentimento => "arquivos-aguardando-consentimento",
            Scenario::PermissoesNoMac => "permissoes-no-mac",
        }
    }

    pub fn from_name(name: &str) -> Option<Scenario> {
        Scenario::ALL.into_iter().find(|s| s.name() == name)
    }

    /// The design board this state comes from, or what the design lacks.
    pub const fn board(self) -> &'static str {
        match self {
            Scenario::Inicio => "new: Início, the home screen (D35)",
            Scenario::InicioEscuro => "new: Início escuro (D35)",
            Scenario::Dispositivos => "1. Dispositivos (Main.dc.html)",
            Scenario::DispositivosEscuro => "1. Dispositivos escuro (MainEscuro.dc.html)",
            Scenario::Permissoes => "3. Permissões por direção",
            Scenario::Historico => "Histórico (placeholder; the design has no board)",
            Scenario::Pareamento => "2. Pareamento (one of the two windows)",
            Scenario::ParearEsteComputador => "new: Parear este computador, 2 min (D12)",
            Scenario::PedidoConexao => "4. Pedido de conexão",
            Scenario::Consentimento => "new: consent card (D13)",
            Scenario::Elevacao => "new: elevation step (D7, D10)",
            Scenario::Atividade => "13. Atividade",
            Scenario::Ajustes => "12. Ajustes",
            Scenario::EstadoRedePublica => "11. Erros e estados: rede Pública",
            Scenario::EstadoServicoMac => "11. Erros e estados: serviço no Mac",
            Scenario::EstadoPermissoesMac => "11. Erros e estados: permissões no Mac",
            Scenario::EstadoReconectando => "11. Erros e estados: Reconectando",
            Scenario::EstadoAcordando => "11. Erros e estados: Acordando",
            Scenario::EstadoConectando => "new: Conectando, before the other computer answers (R-UI-13)",
            Scenario::EstadoAguardandoAceite => "new: Esperando o computador aceitar, with the seconds left (R-UI-13)",
            Scenario::EstadoConexaoRecusada => "new: Não deu para conectar, o outro computador recusou (R-UI-13)",
            Scenario::EstadoConexaoNinguem => "new: Não deu para conectar, ninguém no outro computador (R-UI-13)",
            Scenario::EstadoInversaoRecusada => "11. Erros e estados: inversão recusada",
            Scenario::Visualizador => "5. Visualizador",
            Scenario::VisualizadorEscuro => "5. Visualizador escuro",
            Scenario::Terminal => "8. Terminal e ações rápidas (shell only)",
            Scenario::Arquivos => "9. Arquivos",
            Scenario::Chat => "10. Chat e copiar e colar",
            Scenario::AreaTransferencia => "10. Chat e copiar e colar, the clipboard tab",
            Scenario::Banner => "6. Sendo controlado",
            Scenario::Inverter => "7. Inverter controle, step 1",
            Scenario::InverterPedido => "7. Inverter controle, step 2",
            Scenario::InverterAguardando => {
                "new: Esperando o computador aceitar a inversao, with the seconds left (R-UI-13)"
            }
            Scenario::PareamentoConfirmando => {
                "new: Pareamento, waiting for the other side to confirm the code (R-UI-13)"
            }
            Scenario::EstadoFechouInesperado => "new: crash notice (D12)",
            Scenario::EstadoNenhumComputador => "new: no computer found card (F20)",
            Scenario::EstadoNaoPareado => "new: a computer not paired yet (D51)",
            Scenario::PareadosAntesDa11 => "new: the note under an empty Pareados after the update from 1.0 (D55)",
            Scenario::AtividadeVazia => "new: Atividade with nothing logged (D51)",
            Scenario::Carregando => "R-UI-1: Dispositivos before any answer (placeholder blocks)",
            Scenario::AtividadeCarregando => "R-UI-1: Atividade while the log loads",
            Scenario::ArquivosCarregando => "R-UI-1: Arquivos while the folder lists",
            Scenario::TerminalAguardandoConsentimento => "new: Terminal while the other side's user is asked (R-UI-13)",
            Scenario::ArquivosAguardandoConsentimento => "new: Arquivos while the other side's user is asked (R-UI-13)",
            Scenario::PermissoesNoMac => "new: Permissões on a Mac, the administrator terminal unavailable (D32)",
        }
    }

    /// The events that fill the world every scenario starts from, as the service's first answers would.
    fn world_events(self) -> Vec<UiEvent> {
        match self {
            // Nothing has answered yet.
            Scenario::Carregando => return Vec::new(),
            Scenario::AtividadeCarregando => {
                return vec![
                    reply(Ok(Reply::ThisDevice(this_device()))),
                    reply(Ok(Reply::Network(network(Vec::new(), NetCategoryView::Private)))),
                    reply(Ok(Reply::Devices(devices()))),
                    reply(Ok(Reply::Settings(settings()))),
                ];
            }
            _ => {}
        }
        let logged = if self == Scenario::AtividadeVazia { Vec::new() } else { activity() };
        vec![
            reply(Ok(Reply::ThisDevice(this_device()))),
            reply(Ok(Reply::Network(network(Vec::new(), NetCategoryView::Private)))),
            reply(Ok(Reply::Devices(devices()))),
            reply(Ok(Reply::Settings(settings()))),
            reply(Ok(Reply::Activity(logged))),
        ]
    }

    fn viewer_session(since_ago_min: u64) -> UiEvent {
        Self::viewer_session_to(MAC, since_ago_min)
    }

    fn viewer_session_to(device: &str, since_ago_min: u64) -> UiEvent {
        UiEvent::Session {
            session_id: 1,
            device_id: device.into(),
            state: SessionState::Active {
                since_ms: NOW_MS - since_ago_min * MIN,
                role: Role::Viewer,
                granted: perms(&[
                    Permission::View,
                    Permission::Control,
                    Permission::Audio,
                    Permission::Files,
                    Permission::Clipboard,
                ]),
            },
        }
    }

    fn host_session_to(device: &str) -> UiEvent {
        UiEvent::Session {
            session_id: 2,
            device_id: device.into(),
            state: SessionState::Active {
                since_ms: NOW_MS - 12 * MIN,
                role: Role::Host,
                granted: perms(&[Permission::View, Permission::Control, Permission::Files]),
            },
        }
    }

    /// The script after the common world.
    pub fn script(self) -> Vec<Step> {
        use Step::*;
        let mac = || Select(MAC);
        match self {
            Scenario::Inicio => vec![],
            Scenario::InicioEscuro => vec![Event(UiEvent::ThemeChanged { dark: true })],
            Scenario::Dispositivos => vec![mac()],
            Scenario::DispositivosEscuro => vec![mac(), Event(UiEvent::ThemeChanged { dark: true })],
            Scenario::Permissoes => vec![mac(), Tab(DeviceTab::Permissions)],
            Scenario::Historico => vec![mac(), Tab(DeviceTab::History)],
            Scenario::Pareamento => vec![
                Select(AIR),
                Event(UiEvent::Pairing {
                    pairing_id: 1,
                    state: PairingState::ShowCode { code: "482 913".into(), peer_name: "MacBook Air".into() },
                }),
            ],
            Scenario::ParearEsteComputador => {
                vec![Event(UiEvent::Pairing { pairing_id: 1, state: PairingState::Open { remaining_s: 102 } })]
            }
            Scenario::PedidoConexao => vec![
                mac(),
                Event(UiEvent::IncomingRequest(IncomingRequest {
                    request_id: 7,
                    device: devices().remove(0),
                    kind: RequestKind::Session { intent: Intent::Control },
                    preset: perms(&[Permission::Control, Permission::Audio, Permission::Files, Permission::Clipboard]),
                    timeout_s: 18,
                })),
            ],
            Scenario::Consentimento => vec![
                mac(),
                Event(UiEvent::IncomingRequest(IncomingRequest {
                    request_id: 8,
                    device: devices().remove(0),
                    kind: RequestKind::Consent { feature: Feature::Terminal },
                    preset: perms(&[Permission::Terminal]),
                    timeout_s: 25,
                })),
            ],
            Scenario::Elevacao => vec![
                mac(),
                Tab(DeviceTab::Permissions),
                ActivateRefused(ControlId::Grant(Permission::AdminTerminal), ApiCode::NeedsAdmin),
            ],
            Scenario::Atividade => vec![Goto(Nav::Activity)],
            Scenario::Ajustes => vec![Goto(Nav::Settings)],
            Scenario::EstadoRedePublica => vec![
                mac(),
                Event(UiEvent::NetworkChanged(network(vec![Notice::PublicNetwork], NetCategoryView::Public))),
            ],
            Scenario::EstadoServicoMac => vec![
                mac(),
                Event(UiEvent::NetworkChanged(network(vec![Notice::ServiceNeedsApproval], NetCategoryView::Private))),
            ],
            Scenario::EstadoPermissoesMac => vec![
                mac(),
                Event(UiEvent::NetworkChanged(network(
                    vec![Notice::MacPermissionsMissing(vec![
                        OsPermission::Accessibility,
                        OsPermission::InputMonitoring,
                    ])],
                    NetCategoryView::Private,
                ))),
            ],
            Scenario::EstadoReconectando => vec![
                mac(),
                Event(Self::viewer_session(12)),
                Event(UiEvent::Session {
                    session_id: 1,
                    device_id: MAC.into(),
                    state: SessionState::Reconnecting { attempt: 2 },
                }),
            ],
            Scenario::EstadoAcordando => vec![
                Select(DESKTOP),
                Event(UiEvent::Session {
                    session_id: 3,
                    device_id: DESKTOP.into(),
                    state: SessionState::Waking { remaining_s: 36 },
                }),
            ],
            Scenario::EstadoConectando => vec![mac(), Activate(ControlId::Control)],
            Scenario::EstadoAguardandoAceite => vec![mac(), Event(Self::entry_prompted()), Ticks(6)],
            Scenario::EstadoConexaoRecusada | Scenario::EstadoConexaoNinguem => {
                let why = if self == Scenario::EstadoConexaoRecusada {
                    ConnectWhy::Declined
                } else {
                    ConnectWhy::NobodyPresent
                };
                vec![
                    mac(),
                    Event(Self::entry_prompted()),
                    Event(UiEvent::Session {
                        session_id: ENTRY,
                        device_id: MAC.into(),
                        state: SessionState::NotOpened { why },
                    }),
                ]
            }
            Scenario::EstadoInversaoRecusada => vec![
                mac(),
                Event(Self::viewer_session(12)),
                Event(UiEvent::Swap { session_id: 1, event: SwapEvent::Declined { why: SwapWhy::TimedOut } }),
            ],
            Scenario::Visualizador | Scenario::VisualizadorEscuro => {
                let mut v = vec![
                    mac(),
                    Event(UiEvent::SettingsChanged(Settings { show_stats: true, ..settings() })),
                    Event(Self::viewer_session(12)),
                    Event(UiEvent::Stats { session_id: 1, stats: Stats { latency_ms: 18, fps: 60, kbps: 9000 } }),
                ];
                if self == Scenario::VisualizadorEscuro {
                    v.push(Event(UiEvent::ThemeChanged { dark: true }));
                }
                v
            }
            Scenario::Terminal => vec![
                mac(),
                Event(Self::viewer_session(15)),
                Activate(ControlId::TbTerminal),
                Event(UiEvent::Term { term_id: 1, event: TermEvent::Opened { account: "alex".into() } }),
            ],
            Scenario::Arquivos => vec![
                mac(),
                Event(Self::viewer_session(18)),
                Activate(ControlId::TbFiles),
                Event(UiEvent::Transfer {
                    transfer_id: 1,
                    session_id: 1,
                    name: "fotos-viagem.zip".into(),
                    outgoing: true,
                    state: TransferState::Running {
                        done: 1_100_000_000,
                        total: 1_800_000_000,
                        bytes_per_s: 94_000_000,
                    },
                }),
                Event(UiEvent::Transfer {
                    transfer_id: 2,
                    session_id: 1,
                    name: "instalador.exe".into(),
                    outgoing: true,
                    state: TransferState::Paused { done: 35, total: 100, why: ApiCode::ServiceDown },
                }),
                Event(UiEvent::Transfer {
                    transfer_id: 3,
                    session_id: 1,
                    name: "relatorio-novembro.pdf".into(),
                    outgoing: true,
                    state: TransferState::Done,
                }),
                Event(reply(Ok(Reply::Listing(Listing {
                    path: "Mesa".into(),
                    entries: vec![
                        DirEntryView {
                            name: "contrato-assinado.pdf".into(),
                            is_dir: false,
                            size: 640_000,
                            mtime_ms: at(9, 0, 1),
                        },
                        DirEntryView { name: "Capturas".into(), is_dir: true, size: 12, mtime_ms: at(9, 0, 2) },
                    ],
                    truncated: false,
                })))),
            ],
            Scenario::AreaTransferencia => vec![
                mac(),
                Event(Self::viewer_session(21)),
                Activate(ControlId::TbChat),
                Activate(ControlId::DrawerTab(DrawerTab::Clipboard)),
            ],
            Scenario::Chat => vec![
                mac(),
                Event(Self::viewer_session(21)),
                Activate(ControlId::TbChat),
                Event(UiEvent::Chat {
                    session_id: 1,
                    message: ChatView {
                        msg_id: 1,
                        mine: false,
                        text: "Pode fechar o Xcode? Vou reiniciar o Mac.".into(),
                        sent_at_ms: at(15, 23, 0),
                        read: true,
                    },
                }),
                Event(UiEvent::Chat {
                    session_id: 1,
                    message: ChatView {
                        msg_id: 2,
                        mine: true,
                        text: "Pode, já salvei tudo.".into(),
                        sent_at_ms: at(15, 24, 0),
                        read: false,
                    },
                }),
                Event(UiEvent::ChatRead { session_id: 1, up_to: 2 }),
                Event(UiEvent::Chat {
                    session_id: 1,
                    message: ChatView {
                        msg_id: 3,
                        mine: false,
                        text: "Agora é você que vê meu Mac. Abre o Farol?".into(),
                        sent_at_ms: at(15, 26, 0),
                        read: true,
                    },
                }),
            ],
            // The banner offers "Inverter" only toward a machine of this OS (Windows here): the Desktop.
            Scenario::Banner => vec![Event(Self::host_session_to(DESKTOP))],
            // Swap is offered only toward a machine of this OS (this mock machine is Windows): the Desktop.
            Scenario::Inverter => {
                vec![Select(DESKTOP), Event(Self::viewer_session_to(DESKTOP, 12)), Activate(ControlId::TbSwap)]
            }
            Scenario::InverterPedido => vec![
                Event(Self::host_session_to(DESKTOP)),
                Event(UiEvent::Swap {
                    session_id: 2,
                    event: SwapEvent::Asked { request_id: 11, by: "Desktop do escritório".into(), remaining_s: 14 },
                }),
            ],
            // The request is sent (no backend in `apply_to`: it stays waiting) and 6 s of the 22 have passed.
            Scenario::InverterAguardando => vec![
                Select(DESKTOP),
                Event(Self::viewer_session_to(DESKTOP, 12)),
                Activate(ControlId::TbSwap),
                Activate(ControlId::DialogPrimary),
                Ticks(6),
            ],
            Scenario::PareamentoConfirmando => vec![
                Select(AIR),
                Event(UiEvent::Pairing {
                    pairing_id: 1,
                    state: PairingState::ShowCode { code: "482 913".into(), peer_name: "MacBook Air".into() },
                }),
                Activate(ControlId::PairSame),
            ],
            Scenario::EstadoFechouInesperado => vec![mac(), CrashNotice],
            Scenario::EstadoNenhumComputador => vec![Event(UiEvent::DevicesChanged(Vec::new())), Ticks(10)],
            Scenario::EstadoNaoPareado => vec![Select(AIR)],
            Scenario::PareadosAntesDa11 => vec![
                Event(UiEvent::Reply {
                    ticket: Ticket(0),
                    result: Ok(Reply::ThisDevice(ThisDevice { legacy_note: true, ..this_device() })),
                }),
                Event(UiEvent::DevicesChanged(
                    devices()
                        .into_iter()
                        .filter(|d| matches!(d.status, DeviceStatus::OnNetworkUnpaired { .. }))
                        .collect(),
                )),
                Ticks(10),
            ],
            Scenario::AtividadeVazia => vec![Goto(Nav::Activity)],
            Scenario::Carregando => vec![Startup, ArmPlaceholders],
            Scenario::AtividadeCarregando => vec![Goto(Nav::Activity), BeginFetch(DataKind::Activity), ArmPlaceholders],
            Scenario::ArquivosCarregando => {
                vec![mac(), Event(Self::viewer_session(18)), Activate(ControlId::TbFiles), ArmPlaceholders]
            }
            Scenario::TerminalAguardandoConsentimento => vec![
                mac(),
                Event(Self::viewer_session(15)),
                Activate(ControlId::TbTerminal),
                Event(UiEvent::ConsentPending { session_id: 1, feature: Feature::Terminal, remaining_s: 20 }),
                Ticks(6),
            ],
            Scenario::PermissoesNoMac => vec![
                Event(UiEvent::Reply {
                    ticket: Ticket(0),
                    result: Ok(Reply::ThisDevice(ThisDevice { os: Os::MacOs, ..this_device() })),
                }),
                mac(),
                Tab(DeviceTab::Permissions),
            ],
            Scenario::ArquivosAguardandoConsentimento => vec![
                mac(),
                Event(Self::viewer_session(18)),
                Activate(ControlId::TbFiles),
                Event(UiEvent::ConsentPending { session_id: 1, feature: Feature::Files, remaining_s: 20 }),
                Ticks(6),
                ArmPlaceholders,
            ],
        }
    }

    /// The service's word that the other computer shows the request, with its 20 seconds.
    fn entry_prompted() -> UiEvent {
        UiEvent::Session { session_id: ENTRY, device_id: MAC.into(), state: SessionState::Prompted { remaining_s: 20 } }
    }

    /// Replays the world and the script on `state`.
    pub fn apply_to(self, state: &mut UiState) {
        state.set_clock(Clock { now_ms: NOW_MS, utc_offset_min: 0 });
        for e in self.world_events() {
            state.apply(e);
        }
        let mut next_ticket = 100;
        for step in self.script() {
            match step {
                Step::Event(e) => {
                    state.apply(e);
                }
                Step::Select(id) => {
                    state.select_device(id);
                }
                Step::Tab(t) => {
                    state.select_tab(t);
                }
                Step::Goto(n) => {
                    state.navigate(n);
                }
                Step::Activate(c) => {
                    state.activate(c);
                }
                Step::Startup => {
                    state.startup_commands();
                }
                Step::BeginFetch(k) => state.begin_fetch(k),
                Step::CrashNotice => state.show_crash_notice(),
                Step::Ticks(n) => {
                    for _ in 0..n {
                        state.tick();
                    }
                }
                Step::ArmPlaceholders => {
                    state.arm_placeholders();
                }
                Step::ActivateRefused(c, code) => {
                    let a = state.activate(c);
                    for cmd in a.commands {
                        let t = Ticket(next_ticket);
                        next_ticket += 1;
                        state.track(t, &cmd);
                        state.apply(UiEvent::Reply { ticket: t, result: Err(ApiError::new(code)) });
                    }
                }
            }
        }
    }

    /// A fresh state (pt-BR, light) after the scenario.
    pub fn prepared_state(self) -> UiState {
        let mut s = UiState::default();
        self.apply_to(&mut s);
        s
    }
}

// ---------- the backend ----------

/// What the mock backend's thread remembers between commands.
struct World {
    devices: Vec<DeviceView>,
    settings: Settings,
    next_session: u64,
    /// The entries the user cancelled: their scheduled news does not arrive.
    cancelled: Arc<Mutex<Vec<u64>>>,
}

struct Shared {
    events: Mutex<VecDeque<UiEvent>>,
    waker: Arc<dyn UiWaker>,
}

impl Shared {
    fn push(&self, e: UiEvent) {
        if let Ok(mut q) = self.events.lock() {
            q.push_back(e);
        }
        self.waker.wake();
    }

    fn later(self: &Arc<Self>, delay: Duration, e: UiEvent) {
        let me = Arc::clone(self);
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            me.push(e);
        });
    }

    /// Like [`Shared::later`], unless `entry` was cancelled by then.
    fn later_unless(self: &Arc<Self>, delay: Duration, e: UiEvent, entry: u64, cancelled: &Arc<Mutex<Vec<u64>>>) {
        let (me, cancelled) = (Arc::clone(self), Arc::clone(cancelled));
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if cancelled.lock().is_ok_and(|c| !c.contains(&entry)) {
                me.push(e);
            }
        });
    }
}

/// Faults a test or a demo arms for the next command.
#[derive(Default)]
struct Faults {
    refuse_next: Option<ApiCode>,
    stall_next: bool,
}

/// How long the mock "service" takes for a command: reads are quick, the activity log and folder listings are slow
/// enough to show placeholders (R-UI-1).
pub fn latency(command: &Command) -> Duration {
    Duration::from_millis(match command {
        Command::GetThisDevice | Command::GetNetwork => 40,
        Command::ListDevices | Command::GetSettings => 80,
        Command::ActivityList { .. } => 200,
        Command::FilesList { .. } => 250,
        _ => 40,
    })
}

/// A `UiBackend` that answers from a script. Every command runs on its own short thread after its scripted latency, so
/// independent requests overlap as they would against a service (R-UI-6); identical in-flight commands merge
/// (R-UI-3); `send` only queues and never waits (R-UI-8).
pub struct MockBackend {
    shared: Arc<Shared>,
    world: Arc<Mutex<World>>,
    inflight: Arc<InFlight>,
    faults: Arc<Mutex<Faults>>,
    executed: Arc<AtomicUsize>,
    next_ticket: AtomicU64,
}

impl MockBackend {
    pub fn new(waker: Arc<dyn UiWaker>) -> MockBackend {
        MockBackend {
            shared: Arc::new(Shared { events: Mutex::new(VecDeque::new()), waker }),
            world: Arc::new(Mutex::new(World {
                devices: devices(),
                settings: settings(),
                next_session: 10,
                cancelled: Arc::new(Mutex::new(Vec::new())),
            })),
            inflight: Arc::new(InFlight::new()),
            faults: Arc::new(Mutex::new(Faults::default())),
            executed: Arc::new(AtomicUsize::new(0)),
            next_ticket: AtomicU64::new(1),
        }
    }

    /// The next command is answered with this error (a refusal).
    pub fn refuse_next(&self, code: ApiCode) {
        if let Ok(mut f) = self.faults.lock() {
            f.refuse_next = Some(code);
        }
    }

    /// The next command is never answered (the UI's timeout must fire, R-UI-7).
    pub fn stall_next(&self) {
        if let Ok(mut f) = self.faults.lock() {
            f.stall_next = true;
        }
    }

    /// How many commands actually ran (merged ones do not count).
    pub fn executed(&self) -> usize {
        self.executed.load(Ordering::Relaxed)
    }
}

impl UiBackend for MockBackend {
    fn send(&self, command: Command) -> Ticket {
        let started = std::time::Instant::now();
        let ticket = Ticket(self.next_ticket.fetch_add(1, Ordering::Relaxed));
        if let Joined::Merged(first) = self.inflight.join(&command, ticket) {
            return first;
        }
        let (refuse, stall) =
            self.faults.lock().map_or((None, false), |mut f| (f.refuse_next.take(), std::mem::take(&mut f.stall_next)));
        let (shared, world, inflight, executed) =
            (Arc::clone(&self.shared), Arc::clone(&self.world), Arc::clone(&self.inflight), Arc::clone(&self.executed));
        std::thread::spawn(move || {
            std::thread::sleep(latency(&command));
            if stall {
                inflight.finish(ticket);
                return;
            }
            executed.fetch_add(1, Ordering::Relaxed);
            let result = match refuse {
                Some(code) => Err(ApiError::new(code)),
                None => world
                    .lock()
                    .map_or_else(|_| Err(ApiError::new(ApiCode::Internal)), |mut w| w.handle(&shared, command)),
            };
            inflight.finish(ticket);
            shared.push(UiEvent::Reply { ticket, result });
        });
        debug_assert!(started.elapsed() < Duration::from_millis(50), "MockBackend::send blocked the caller");
        ticket
    }

    fn drain(&self) -> Vec<UiEvent> {
        self.shared.events.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
    }
}

impl World {
    fn device(&mut self, id: &str) -> Result<&mut DeviceView, ApiError> {
        self.devices.iter_mut().find(|d| d.id == id).ok_or_else(|| ApiError::new(ApiCode::NotFound))
    }

    fn handle(&mut self, sh: &Arc<Shared>, command: Command) -> Result<Reply, ApiError> {
        let ms = Duration::from_millis;
        match command {
            Command::GetThisDevice => Ok(Reply::ThisDevice(this_device())),
            Command::GetNetwork => Ok(Reply::Network(network(Vec::new(), NetCategoryView::Private))),
            Command::ListDevices => Ok(Reply::Devices(self.devices.clone())),
            Command::GetSettings => Ok(Reply::Settings(self.settings.clone())),
            Command::ActivityList { .. } => Ok(Reply::Activity(activity())),
            Command::SetSettings { settings } => {
                self.settings = settings.clone();
                Ok(Reply::Settings(settings))
            }
            Command::PairOpen { device_id } => {
                match device_id {
                    Some(_) => sh.later(
                        ms(500),
                        UiEvent::Pairing {
                            pairing_id: 1,
                            state: PairingState::ShowCode { code: "482 913".into(), peer_name: "MacBook Air".into() },
                        },
                    ),
                    None => {
                        sh.later(
                            ms(100),
                            UiEvent::Pairing { pairing_id: 1, state: PairingState::Open { remaining_s: 120 } },
                        );
                        sh.later(
                            ms(4000),
                            UiEvent::Pairing {
                                pairing_id: 1,
                                state: PairingState::ShowCode {
                                    code: "482 913".into(),
                                    peer_name: "MacBook Air".into(),
                                },
                            },
                        );
                    }
                }
                Ok(Reply::PairingId(1))
            }
            Command::PairConfirm { matches: true, .. } => {
                if let Ok(d) = self.device(AIR) {
                    d.paired = true;
                    d.status = DeviceStatus::Online { address: "192.168.0.34".into(), rtt_ms: 4 };
                    d.grants_there = Some(Permissions::NONE);
                }
                sh.later(ms(600), UiEvent::DevicesChanged(self.devices.clone()));
                sh.later(
                    ms(700),
                    UiEvent::Pairing { pairing_id: 1, state: PairingState::Done { device_id: AIR.into() } },
                );
                Ok(Reply::Done)
            }
            Command::SetGrants { device_id, grants } => {
                let elevated_needed = {
                    let d = self.device(&device_id)?;
                    let added = grants.0 & !d.grants_here.0;
                    added & (Permission::Unattended.bit() | Permission::AdminTerminal.bit()) != 0
                };
                if elevated_needed {
                    return Err(ApiError::new(ApiCode::NeedsAdmin));
                }
                let d = self.device(&device_id)?;
                d.grants_here = grants;
                Ok(Reply::Device(d.clone()))
            }
            Command::ForgetDevice { device_id } => {
                self.device(&device_id)?;
                self.devices.retain(|d| d.id != device_id);
                Ok(Reply::Done)
            }
            Command::RefreshGrants { device_id } => {
                self.device(&device_id)?;
                Ok(Reply::Done)
            }
            Command::RenameDevice { device_id, name } => {
                let name = name.trim();
                if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
                    return Err(ApiError::new(ApiCode::Invalid));
                }
                let d = self.device(&device_id)?;
                if !d.paired {
                    return Err(ApiError::new(ApiCode::NotFound));
                }
                d.name = name.to_owned();
                let devices = self.devices.clone();
                sh.later(ms(50), UiEvent::DevicesChanged(devices));
                Ok(Reply::Done)
            }
            Command::Elevate { action, run, .. } => {
                if let ElevatedAction::SetGrants { device_id, grants } = &action
                    && let Ok(d) = self.device(device_id)
                {
                    d.grants_here = *grants;
                }
                let devices = self.devices.clone();
                sh.later(ms(900), UiEvent::ElevationResult { action, run, result: Ok(()) });
                sh.later(ms(950), UiEvent::DevicesChanged(devices));
                Ok(Reply::Done)
            }
            Command::Connect { device_id, .. } => {
                self.device(&device_id)?;
                self.next_session += 1;
                let session = self.next_session;
                let entry = ENTRY_BIT | session;
                let cancelled = Arc::clone(&self.cancelled);
                let say =
                    |state: SessionState| UiEvent::Session { session_id: entry, device_id: device_id.clone(), state };
                sh.later_unless(ms(10), say(SessionState::Connecting), entry, &cancelled);
                sh.later_unless(ms(300), say(SessionState::Prompted { remaining_s: 20 }), entry, &cancelled);
                // The other computer accepted: the session's own Active first, then the entry's normal end.
                sh.later_unless(
                    ms(900),
                    UiEvent::Session {
                        session_id: session,
                        device_id: device_id.clone(),
                        state: SessionState::Active {
                            since_ms: NOW_MS,
                            role: Role::Viewer,
                            granted: perms(&[
                                Permission::View,
                                Permission::Control,
                                Permission::Audio,
                                Permission::Files,
                                Permission::Clipboard,
                            ]),
                        },
                    },
                    entry,
                    &cancelled,
                );
                sh.later_unless(ms(920), say(SessionState::Ended { reason: None }), entry, &cancelled);
                Ok(Reply::SessionId(entry))
            }
            Command::Disconnect { session_id } => {
                if session_id & ENTRY_BIT != 0
                    && let Ok(mut c) = self.cancelled.lock()
                {
                    c.push(session_id);
                }
                sh.later(
                    ms(20),
                    UiEvent::Session {
                        session_id,
                        device_id: String::new(),
                        state: SessionState::Ended { reason: None },
                    },
                );
                Ok(Reply::Done)
            }
            Command::RequestSwap { .. } => {
                sh.later(
                    ms(1500),
                    UiEvent::Swap { session_id: 0, event: SwapEvent::Declined { why: SwapWhy::TimedOut } },
                );
                Ok(Reply::RequestId(9))
            }
            Command::TermOpen { .. } => {
                sh.later(ms(100), UiEvent::Term { term_id: 1, event: TermEvent::Opened { account: "alex".into() } });
                Ok(Reply::TermId(1))
            }
            Command::FilesList { .. } => {
                Ok(Reply::Listing(Listing { path: "Mesa".into(), entries: Vec::new(), truncated: false }))
            }
            Command::ChatSend { text, .. } => {
                Ok(Reply::Chat(ChatView { msg_id: 100, mine: true, text, sent_at_ms: NOW_MS, read: false }))
            }
            Command::ActivityExport { path } => {
                // [UI] work: the UI process writes the exported log itself, here on the backend thread.
                let lang = Language::PtBr;
                let mut csv = String::from(
                    "quando;dispositivo;o que aconteceu;duracao_s
",
                );
                for r in activity() {
                    let when = crate::format::table_when(
                        lang,
                        r.at_ms,
                        crate::format::Clock { now_ms: NOW_MS, utc_offset_min: 0 },
                    );
                    let what = crate::strings::text(lang, &r.what);
                    csv.push_str(&format!(
                        "{when};{};{what};{}
",
                        r.device_name,
                        r.duration_s.map_or(String::new(), |d| d.to_string())
                    ));
                }
                std::fs::write(&path, csv).map(|()| Reply::Done).map_err(|_| ApiError::new(ApiCode::Internal))
            }
            Command::RequestOsPermissions => Ok(Reply::MissingPermissions(Vec::new())),
            Command::ListDisplays { .. } => Ok(Reply::Displays(vec![DisplayView {
                id: 1,
                name: "Tela 1".into(),
                width: 2560,
                height: 1600,
                scale_milli: 2000,
                primary: true,
            }])),
            _ => Ok(Reply::Done),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{DialogKind, EditId, Screen, WindowKind};

    #[test]
    fn scenario_names_are_unique_and_round_trip() {
        for s in Scenario::ALL {
            assert_eq!(Scenario::from_name(s.name()), Some(s));
        }
        let mut names: Vec<_> = Scenario::ALL.iter().map(|s| s.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Scenario::ALL.len());
        assert_eq!(Scenario::from_name("nao-existe"), None);
    }

    /// (scenario, main screen, windows that must exist)
    #[test]
    fn every_scenario_drives_the_state_to_its_screen() {
        use WindowKind::*;
        let expect: &[(Scenario, Screen, &[WindowKind])] = &[
            (Scenario::Inicio, Screen::Home, &[Main]),
            (Scenario::InicioEscuro, Screen::Home, &[Main]),
            (Scenario::Dispositivos, Screen::Devices, &[Main]),
            (Scenario::DispositivosEscuro, Screen::Devices, &[Main]),
            (Scenario::Permissoes, Screen::Devices, &[Main]),
            (Scenario::Historico, Screen::Devices, &[Main]),
            (Scenario::Pareamento, Screen::Pairing, &[Main]),
            (Scenario::ParearEsteComputador, Screen::Pairing, &[Main]),
            (Scenario::PedidoConexao, Screen::Devices, &[Main]),
            (Scenario::Consentimento, Screen::Devices, &[Main]),
            (Scenario::Elevacao, Screen::Devices, &[Main]),
            (Scenario::Atividade, Screen::Activity, &[Main]),
            (Scenario::Ajustes, Screen::Settings, &[Main]),
            (Scenario::EstadoRedePublica, Screen::Devices, &[Main]),
            (Scenario::EstadoServicoMac, Screen::Devices, &[Main]),
            (Scenario::EstadoPermissoesMac, Screen::Devices, &[Main]),
            (Scenario::EstadoReconectando, Screen::Devices, &[Main, Viewer]),
            (Scenario::EstadoAcordando, Screen::Devices, &[Main]),
            (Scenario::EstadoConectando, Screen::Devices, &[Main]),
            (Scenario::EstadoAguardandoAceite, Screen::Devices, &[Main]),
            (Scenario::EstadoConexaoRecusada, Screen::Devices, &[Main]),
            (Scenario::EstadoConexaoNinguem, Screen::Devices, &[Main]),
            (Scenario::EstadoInversaoRecusada, Screen::Devices, &[Main, Viewer]),
            (Scenario::Visualizador, Screen::Devices, &[Main, Viewer]),
            (Scenario::VisualizadorEscuro, Screen::Devices, &[Main, Viewer]),
            (Scenario::Terminal, Screen::Devices, &[Main, Viewer]),
            (Scenario::Arquivos, Screen::Devices, &[Main, Viewer]),
            (Scenario::Chat, Screen::Devices, &[Main, Viewer]),
            (Scenario::AreaTransferencia, Screen::Devices, &[Main, Viewer]),
            (Scenario::Banner, Screen::Home, &[Main, Banner]),
            (Scenario::Inverter, Screen::Devices, &[Main, Viewer]),
            (Scenario::InverterPedido, Screen::Home, &[Main, Banner]),
            (Scenario::InverterAguardando, Screen::Devices, &[Main, Viewer]),
            (Scenario::PareamentoConfirmando, Screen::Pairing, &[Main]),
            (Scenario::EstadoFechouInesperado, Screen::Devices, &[Main]),
            (Scenario::EstadoNenhumComputador, Screen::Home, &[Main]),
            (Scenario::EstadoNaoPareado, Screen::Devices, &[Main]),
            (Scenario::AtividadeVazia, Screen::Activity, &[Main]),
            (Scenario::Carregando, Screen::Home, &[Main]),
            (Scenario::AtividadeCarregando, Screen::Activity, &[Main]),
            (Scenario::ArquivosCarregando, Screen::Devices, &[Main, Viewer]),
            (Scenario::TerminalAguardandoConsentimento, Screen::Devices, &[Main, Viewer]),
            (Scenario::ArquivosAguardandoConsentimento, Screen::Devices, &[Main, Viewer]),
            (Scenario::PermissoesNoMac, Screen::Devices, &[Main]),
            (Scenario::PareadosAntesDa11, Screen::Home, &[Main]),
        ];
        assert_eq!(expect.len(), Scenario::ALL.len());
        for (sc, screen, windows) in expect {
            let s = sc.prepared_state();
            assert_eq!(s.screen(), *screen, "{}", sc.name());
            assert_eq!(s.windows(), *windows, "{}", sc.name());
            assert!(!s.controls(WindowKind::Main).is_empty(), "{} has no controls", sc.name());
        }
    }

    #[test]
    fn the_new_states_show_their_overlays() {
        let s = Scenario::PedidoConexao.prepared_state();
        assert!(s.incoming_session_open());
        assert_eq!(s.dialog(WindowKind::Main), None);

        let s = Scenario::Consentimento.prepared_state();
        assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::Consent));

        let s = Scenario::Elevacao.prepared_state();
        assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::Elevation));
        // The refused toggle goes back to what the service holds.
        let mac = s.device_by_id(MAC).unwrap();
        assert!(!mac.grants_here.has(Permission::AdminTerminal));

        let s = Scenario::ParearEsteComputador.prepared_state();
        assert_eq!(s.pairing().unwrap().state, PairingState::Open { remaining_s: 102 });
        assert!(s.needs_tick());

        let s = Scenario::Inverter.prepared_state();
        assert_eq!(s.dialog(WindowKind::Viewer), Some(DialogKind::SwapConfirm));
        let s = Scenario::InverterPedido.prepared_state();
        assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::SwapAsk));
    }

    #[test]
    fn the_dark_scenarios_are_dark() {
        assert!(Scenario::DispositivosEscuro.prepared_state().dark());
        assert!(Scenario::VisualizadorEscuro.prepared_state().dark());
        assert!(!Scenario::Dispositivos.prepared_state().dark());
    }

    struct NoWake;
    impl UiWaker for NoWake {
        fn wake(&self) {}
    }

    fn wait_for(b: &MockBackend, n: usize) -> Vec<UiEvent> {
        let mut got = Vec::new();
        for _ in 0..200 {
            got.extend(b.drain());
            if got.len() >= n {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        got
    }

    // ----- the responsiveness rules (SPEC R-UI-*), against the mock's scripted delays -----

    /// Sends `commands`, tracks their tickets in `state` and applies every reply that arrives within `limit`.
    fn run(state: &mut UiState, b: &MockBackend, commands: Vec<Command>, limit: Duration) -> Duration {
        let start = std::time::Instant::now();
        let mut waiting = commands.len();
        for c in commands {
            let t = b.send(c.clone());
            state.track(t, &c);
        }
        while waiting > 0 && start.elapsed() < limit {
            for e in b.drain() {
                if matches!(e, UiEvent::Reply { .. }) {
                    waiting -= 1;
                }
                state.apply(e);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        start.elapsed()
    }

    #[test]
    fn r_ui_6_the_launch_requests_overlap_and_the_window_waits_with_placeholders() {
        let b = MockBackend::new(Arc::new(NoWake));
        let mut s = UiState::default();
        let commands = s.startup_commands();
        assert_eq!(commands.len(), 5, "identity, network, devices, settings and activity are asked together");
        // Before any answer nothing is drawn in between (R-UI-1) ...
        assert!(s.has_pending_fetch());
        assert!(!s.placeholder(DataKind::Devices));
        // ... and after 100 ms of silence the placeholders show.
        s.arm_placeholders();
        assert!(s.placeholder(DataKind::Devices) && s.placeholder(DataKind::Activity));
        let sequential: u128 = commands.iter().map(|c| latency(c).as_millis()).sum();
        let took = run(&mut s, &b, commands, Duration::from_secs(3));
        // A sleep never ends early, so answering one after the other can never take less than the sum: finishing
        // under it proves the requests overlapped, however slow the machine (a CI runner overshoots every sleep).
        assert!(took.as_millis() < sequential, "took {took:?}, one after the other would take {sequential} ms");
        for k in [DataKind::ThisDevice, DataKind::Network, DataKind::Devices, DataKind::Settings, DataKind::Activity] {
            assert_eq!(s.fetch(k), crate::state::Fetch::Ready, "{k:?}");
        }
        assert_eq!(s.devices().len(), 3);
        assert!(!s.has_pending_fetch());
    }

    #[test]
    fn r_ui_1_fast_answers_never_show_placeholders() {
        let mut s = UiState::default();
        s.startup_commands();
        // Everything answers before the 100 ms timer: the placeholders are never armed.
        Scenario::Dispositivos.apply_to(&mut s);
        assert_eq!(s.arm_placeholders(), crate::state::Changed::NONE);
        assert!(!s.placeholder(DataKind::Devices));
    }

    #[test]
    fn the_loading_scenarios_show_placeholders() {
        let s = Scenario::Carregando.prepared_state();
        for k in [DataKind::Devices, DataKind::Settings, DataKind::Activity, DataKind::ThisDevice] {
            assert!(s.placeholder(k), "{k:?}");
        }
        assert!(s.devices().is_empty(), "no data yet");
        let s = Scenario::AtividadeCarregando.prepared_state();
        assert!(s.placeholder(DataKind::Activity) && !s.placeholder(DataKind::Devices));
        let s = Scenario::ArquivosCarregando.prepared_state();
        assert!(s.placeholder(DataKind::Listing));
        assert!(!s.controls(WindowKind::Main).is_empty() && !s.controls(WindowKind::Viewer).is_empty());
    }

    #[test]
    fn r_ui_3_identical_commands_in_flight_merge_and_the_cache_answers_the_rest() {
        let b = MockBackend::new(Arc::new(NoWake));
        let (t1, t2, t3) = (b.send(Command::ListDevices), b.send(Command::ListDevices), b.send(Command::ListDevices));
        assert_eq!((t1, t2), (t1, t1), "merged into the first ticket");
        assert_eq!(t3, t1);
        let events = wait_for(&b, 1);
        std::thread::sleep(Duration::from_millis(150));
        let mut all = events;
        all.extend(b.drain());
        assert_eq!(all.len(), 1, "one reply answers all three");
        assert_eq!(b.executed(), 1);
        // A different command is not merged.
        let t4 = b.send(Command::GetSettings);
        assert_ne!(t4, t1);

        // The cache: Atividade and Ajustes are asked once, then kept fresh by events.
        let mut s = Scenario::Dispositivos.prepared_state();
        let a = s.activate(ControlId::HomeActivityAll);
        assert!(a.commands.is_empty(), "the activity log is already cached");
        let a = s.activate(ControlId::NavSettings);
        assert!(a.commands.is_empty(), "the settings are already cached");
        let mut fresh = UiState::default();
        let a = fresh.activate(ControlId::HomeActivityAll);
        assert_eq!(a.commands.len(), 1, "first open asks once");
        let a = fresh.activate(ControlId::NavSettings);
        assert_eq!(a.commands, vec![Command::GetSettings]);
        let again = fresh.activate(ControlId::HomeActivityAll);
        assert!(again.commands.is_empty(), "a second visit does not ask again while the first is in flight");
        // An event refreshes the cache without a request.
        let mut devices = devices();
        devices[0].name = "MacBook renomeado".into();
        s.apply(UiEvent::DevicesChanged(devices));
        assert_eq!(s.devices()[0].name, "MacBook renomeado");
    }

    #[test]
    fn r_ui_2_a_switch_moves_at_once_and_goes_back_when_the_service_refuses() {
        let b = MockBackend::new(Arc::new(NoWake));
        let mut s = Scenario::Permissoes.prepared_state();
        assert!(s.device_by_id(MAC).unwrap().grants_here.has(Permission::Audio));
        let a = s.activate(ControlId::Grant(Permission::Audio));
        assert!(!s.device_by_id(MAC).unwrap().grants_here.has(Permission::Audio), "moved before any answer");
        assert_eq!(a.commands.len(), 1);
        b.refuse_next(ApiCode::Refused);
        run(&mut s, &b, a.commands, Duration::from_secs(2));
        assert!(s.device_by_id(MAC).unwrap().grants_here.has(Permission::Audio), "reverted on refusal");
        assert_eq!(s.error().map(|e| e.code), Some(ApiCode::Refused), "and it says why");

        // Accepted: it stays.
        let mut s = Scenario::Permissoes.prepared_state();
        let a = s.activate(ControlId::Grant(Permission::Audio));
        run(&mut s, &b, a.commands, Duration::from_secs(2));
        assert!(!s.device_by_id(MAC).unwrap().grants_here.has(Permission::Audio));
        assert!(s.error().is_none());
    }

    #[test]
    fn r_ui_2_settings_and_viewer_switches_are_optimistic_too() {
        let b = MockBackend::new(Arc::new(NoWake));
        let mut s = Scenario::Ajustes.prepared_state();
        assert!(s.settings().unwrap().lock_on_end);
        let a = s.activate(ControlId::LockOnEnd);
        assert!(!s.settings().unwrap().lock_on_end);
        b.refuse_next(ApiCode::Busy);
        run(&mut s, &b, a.commands, Duration::from_secs(2));
        assert!(s.settings().unwrap().lock_on_end, "settings revert on refusal");

        let mut s = Scenario::Visualizador.prepared_state();
        assert!(s.audio_on());
        let a = s.activate(ControlId::TbAudio);
        assert!(!s.audio_on());
        b.refuse_next(ApiCode::NotPermitted);
        run(&mut s, &b, a.commands, Duration::from_secs(2));
        assert!(s.audio_on(), "audio reverts on refusal");
    }

    #[test]
    fn r_ui_2_admin_grants_forget_and_pairing_confirmation_wait_for_the_service() {
        let b = MockBackend::new(Arc::new(NoWake));
        let mut s = Scenario::Permissoes.prepared_state();
        // An admin grant does not move first.
        let a = s.activate(ControlId::Grant(Permission::AdminTerminal));
        assert!(!s.device_by_id(MAC).unwrap().grants_here.has(Permission::AdminTerminal));
        let cmd = a.commands[0].clone();
        let t = b.send(cmd.clone());
        s.track(t, &cmd);
        assert!(s.grant_waiting(Permission::AdminTerminal), "the switch waits while the command is out");
        for e in wait_for(&b, 1) {
            s.apply(e);
        }
        assert!(!s.grant_waiting(Permission::AdminTerminal));
        assert_eq!(s.dialog(WindowKind::Main), Some(DialogKind::Elevation), "the service asked for an administrator");

        // Forget waits for Done.
        let mut s = Scenario::Permissoes.prepared_state();
        let a = s.activate(ControlId::Forget);
        assert!(s.device_by_id(MAC).is_some(), "still there before the answer");
        run(&mut s, &b, a.commands, Duration::from_secs(2));
        assert!(s.device_by_id(MAC).is_none());

        // The pairing buttons wait, they do not pretend.
        let mut s = Scenario::Pareamento.prepared_state();
        let a = s.activate(ControlId::PairSame);
        assert_eq!(a.commands.len(), 1);
        assert!(matches!(s.pairing().unwrap().state, PairingState::ShowCode { .. }), "still showing the code");
        // The two buttons give way to "Cancelar" alone (docs/WAITING.md row 7).
        let ids: Vec<_> = s.controls(WindowKind::Main).into_iter().map(|c| c.id).collect();
        assert!(!ids.contains(&ControlId::PairSame) && !ids.contains(&ControlId::PairDifferent));
        assert!(ids.contains(&ControlId::PairCancel));
        assert!(s.activate(ControlId::PairSame).commands.is_empty(), "a second press does nothing");
    }

    #[test]
    fn r_ui_7_a_command_nobody_answers_ends_in_a_visible_failure() {
        let b = MockBackend::new(Arc::new(NoWake));
        let mut s = UiState::default();
        let commands = s.startup_commands();
        b.stall_next();
        let first = commands[0].clone();
        let t = b.send(first.clone());
        s.track(t, &first);
        assert!(s.needs_tick(), "a waiting command keeps the one-second timer on");
        for _ in 0..4 {
            s.tick();
        }
        assert!(s.error().is_none(), "not yet: the limit is 5 s");
        s.tick();
        assert_eq!(s.error().map(|e| e.code), Some(ApiCode::Timeout));
        assert_eq!(
            s.fetch(DataKind::ThisDevice),
            crate::state::Fetch::Missing,
            "its placeholder does not spin forever"
        );
        assert!(!s.needs_tick(), "and the timer stops");
        assert!(s.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::ErrorClose), "with a way to dismiss it");
    }

    #[test]
    fn r_ui_7_a_refused_admin_prompt_or_a_slow_connect_have_their_own_limits() {
        assert_eq!(
            Command::Elevate {
                action: ElevatedAction::OpenAdminTerminal { session_id: 1 },
                line: String::new(),
                run: 1
            }
            .timeout_s(),
            120
        );
        // Connect and Wake answer at once with the entry's id: the wait is told by notifications, so 5 s is for the
        // reply and the interface's own 60 s safety net (`CONNECT_GIVE_UP_S`) is for news that never comes.
        assert_eq!(Command::Connect { device_id: "x".into(), intent: Intent::View, unattended: false }.timeout_s(), 5);
        assert_eq!(Command::Wake { device_id: "x".into() }.timeout_s(), 5);
        assert_eq!(crate::tokens::layout::CONNECT_GIVE_UP_S, 60);
        assert_eq!(Command::ListDevices.timeout_s(), 5);
    }

    #[test]
    fn the_connect_scenarios_show_their_cards() {
        use crate::state::{WaitIcon, WaitPhase};
        let card = |sc: Scenario| {
            let s = sc.prepared_state();
            let card = s.wait_card(MAC).unwrap_or_else(|| panic!("{} has a card", sc.name()));
            let name = s.device_by_id(MAC).unwrap().name.clone();
            (card, name)
        };
        let (c, name) = card(Scenario::EstadoConectando);
        assert_eq!((c.phase, c.icon), (WaitPhase::Connecting, WaitIcon::Clock));
        assert_eq!(c.title, format!("Conectando ao {name}"));
        // The click's card has no id yet, and the only button is Cancelar.
        let s = Scenario::EstadoConectando.prepared_state();
        assert_eq!(s.connect_flow(MAC).unwrap().entry, 0);
        assert!(s.controls(WindowKind::Main).iter().any(|c| c.id == ControlId::ConnectCancel));

        let (c, name) = card(Scenario::EstadoAguardandoAceite);
        assert_eq!(c.phase, WaitPhase::Prompted);
        assert_eq!(c.title, format!("Esperando o {name} aceitar"));
        assert_eq!(c.bar.unwrap().text.as_deref(), Some("0:14"));

        let (c, name) = card(Scenario::EstadoConexaoRecusada);
        assert_eq!((c.phase, c.icon), (WaitPhase::Ended, WaitIcon::ShieldAlert));
        assert_eq!(c.body, format!("O {name} recusou."));

        let (c, name) = card(Scenario::EstadoConexaoNinguem);
        assert_eq!(c.body, format!("Não há ninguém no {name} agora."));
        assert!(c.buttons.iter().any(|b| b.id == ControlId::ConnectRetry));
        for sc in [
            Scenario::EstadoConectando,
            Scenario::EstadoAguardandoAceite,
            Scenario::EstadoConexaoRecusada,
            Scenario::EstadoConexaoNinguem,
        ] {
            assert!(Scenario::ALL.contains(&sc) && Scenario::from_name(sc.name()) == Some(sc));
            assert!(sc.name().starts_with("estado-") && !sc.board().is_empty());
        }
    }

    #[test]
    fn the_mock_connect_answers_at_once_then_reports_the_wait_and_the_session_before_the_entry_ends() {
        let b = MockBackend::new(Arc::new(NoWake));
        let t = b.send(Command::Connect { device_id: MAC.into(), intent: Intent::Control, unattended: false });
        let events = wait_for(&b, 5);
        let entry = match &events[0] {
            UiEvent::Reply { ticket, result: Ok(Reply::SessionId(id)) } if *ticket == t => *id,
            other => panic!("the reply comes first: {other:?}"),
        };
        assert!(entry & (1 << 63) != 0, "an entry id has bit 63 set");
        let states: Vec<(u64, &SessionState)> = events[1..]
            .iter()
            .map(|e| match e {
                UiEvent::Session { session_id, state, .. } => (*session_id, state),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(states.len(), 4, "{states:?}");
        assert_eq!(states[0], (entry, &SessionState::Connecting));
        assert_eq!(states[1], (entry, &SessionState::Prompted { remaining_s: 20 }));
        assert!(matches!(states[2], (id, SessionState::Active { role: Role::Viewer, .. }) if id != entry));
        assert_eq!(states[3], (entry, &SessionState::Ended { reason: None }));
    }

    #[test]
    fn a_cancelled_mock_connect_opens_no_session() {
        let b = MockBackend::new(Arc::new(NoWake));
        b.send(Command::Connect { device_id: MAC.into(), intent: Intent::Control, unattended: false });
        let events = wait_for(&b, 1);
        let UiEvent::Reply { result: Ok(Reply::SessionId(entry)), .. } = &events[0] else { panic!("{events:?}") };
        b.send(Command::Disconnect { session_id: *entry });
        std::thread::sleep(Duration::from_millis(1300));
        let later = b.drain();
        assert!(
            !later.iter().any(|e| matches!(e, UiEvent::Session { state: SessionState::Active { .. }, .. })),
            "{later:?}"
        );
        assert!(later.iter().any(|e| matches!(
            e,
            UiEvent::Session { session_id, state: SessionState::Ended { reason: None }, .. } if session_id == entry
        )));
    }

    #[test]
    fn r_ui_4_the_address_field_acts_150_ms_after_the_last_key() {
        use crate::debounce::Debouncer;
        let b = MockBackend::new(Arc::new(NoWake));
        let mut s = Scenario::Dispositivos.prepared_state();
        let mut d = Debouncer::new();
        let typed = ["1", "19", "192.", "192.168.0.", "192.168.0.40"];
        for (i, t) in typed.iter().enumerate() {
            d.input(t, 1000 + i as u64 * 40);
            assert_eq!(d.poll(1000 + i as u64 * 40 + 10), None, "no request while typing");
        }
        let last = 1000 + 4 * 40;
        assert_eq!(d.poll(last + 149), None);
        let text = d.poll(last + 150).expect("acts 150 ms after the last key");
        let a = s.submit_edit(EditId::Address, text);
        assert_eq!(a.commands, vec![Command::AddByAddress { ipv4: "192.168.0.40".into() }]);
        run(&mut s, &b, a.commands, Duration::from_secs(2));
        assert!(
            s.submit_edit(EditId::Address, "192.168.0.".into()).commands.is_empty(),
            "a half-typed address is not sent"
        );
        // Chat is never on a timer: only Enter (the shell calls submit_edit then).
        assert!(s.submit_edit(EditId::Chat, "  ".into()).commands.is_empty());
    }

    #[test]
    fn r_ui_8_send_never_waits_for_the_answer() {
        let b = MockBackend::new(Arc::new(NoWake));
        let t = std::time::Instant::now();
        b.send(Command::ActivityList { before_ms: None, limit: 10 });
        assert!(t.elapsed() < Duration::from_millis(30), "send took {:?} for a 200 ms command", t.elapsed());
    }

    #[test]
    fn the_backend_answers_every_command_with_a_reply_for_its_ticket() {
        let b = MockBackend::new(Arc::new(NoWake));
        let t = b.send(Command::ListDevices);
        let events = wait_for(&b, 1);
        match &events[0] {
            UiEvent::Reply { ticket, result: Ok(Reply::Devices(d)) } => {
                assert_eq!(*ticket, t);
                assert_eq!(d.len(), 3);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn granting_unattended_needs_an_administrator_until_elevated() {
        let b = MockBackend::new(Arc::new(NoWake));
        let grants = devices()[1].grants_here.with(Permission::Unattended);
        let t = b.send(Command::SetGrants { device_id: DESKTOP.into(), grants });
        let events = wait_for(&b, 1);
        assert!(
            matches!(&events[0], UiEvent::Reply { ticket, result: Err(e) } if *ticket == t && e.code == ApiCode::NeedsAdmin)
        );
    }
}
