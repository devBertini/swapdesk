//! Every string the interface shows, keyed, in pt-BR and en. The shells hold no literal text (C6).
//!
//! One table declares each key with both translations, so a key without a language cannot exist. Arguments are
//! `{0}`, `{1}`, ... and are replaced by [`text`].

use crate::api::{Language, Msg};

macro_rules! strings {
    ($($key:ident => $pt:literal, $en:literal;)*) => {
        /// One variant per string of the interface.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum MsgKey { $($key,)* }

        impl MsgKey {
            /// Every key, in declaration order.
            pub const ALL: &'static [MsgKey] = &[$(MsgKey::$key,)*];

            /// The template of a key in a language, with its `{n}` placeholders.
            pub const fn template(self, lang: Language) -> &'static str {
                match (self, lang) {
                    $((MsgKey::$key, Language::PtBr) => $pt, (MsgKey::$key, Language::En) => $en,)*
                }
            }
        }
    };
}

strings! {
    AppName => "Swapdesk", "Swapdesk";

    // Sidebar
    SidebarThisComputer => "Este computador", "This computer";
    SidebarPaired => "Pareados", "Paired";
    SidebarOnNetwork => "Na rede", "On the network";
    AddressLabel => "Adicionar por endereço", "Add by address";
    AddressPlaceholder => "192.168.0.21", "192.168.0.21";
    AddressRefusedWhy => "Só endereços desta rede local servem: não o de outra rede, não o de loopback e não o deste computador.", "Only addresses on this local network work, not ones from another network, loopback addresses or this computer’s own.";
    SidebarNavLabel => "Dispositivos", "Devices";
    NavHome => "Início", "Home";
    NavSettings => "Ajustes", "Settings";
    NetworkLine => "Só na rede local: {0}, {1}", "Local network only: {0}, {1}";
    NetworkLineNoAddress => "Só na rede local: {0}", "Local network only: {0}";
    NoNetwork => "Sem rede", "No network";
    PairThisComputer => "Parear este computador", "Pair this computer";
    ReadyToReceive => "Pronto para receber", "Ready to receive";
    ReadyServiceOff => "Serviço desligado", "Service is off";
    ReadyServiceNeedsApproval => "Serviço espera aprovação", "Service waits for approval";
    ReadyNoNetwork => "Sem rede", "No network";
    StatusOnline => "Online, {0}", "Online, {0}";
    StatusAsleep => "Dormindo, {0}", "Asleep, {0}";
    StatusOffline => "Fora da rede, {0}", "Offline, {0}";
    StatusUnpaired => "Não pareado", "Not paired";
    StatusUnpairedOpen => "Não pareado, pareamento aberto", "Not paired, pairing open";
    OsLabel => "{0} {1}", "{0} {1}";
    DeviceCaptionThis => "Este computador", "This computer";
    EmptyDevices => "Nenhum computador encontrado ainda.", "No computers found yet.";

    // Home (D35)
    HomePairTitle => "Parear", "Pair";
    HomePairBody => "Os dois computadores se conhecem uma vez, conferindo o mesmo código de 6 dígitos nas duas telas. Depois disso, um pode ver e controlar o outro.", "The two computers get to know each other once, by checking the same 6-digit code on both screens. After that, one can see and control the other.";
    HomeAddressHint => "Se o outro computador não aparece em Na rede, digite o IPv4 dele.", "If the other computer does not show up under “On the network”, type its IPv4 address.";
    HomeAddressAdd => "Adicionar", "Add";
    HomeRecentTitle => "Atividade recente", "Recent activity";
    HomeRecentEmpty => "Ainda não aconteceu nada por aqui.", "Nothing has happened here yet.";
    HomeRecentAll => "Ver tudo", "See all";

    // Device header and tabs
    DetailOnline => "Online em {0}, resposta em {1} ms", "Online at {0}, {1} ms response";
    DetailAsleep => "Dormindo. Dá para acordar pela rede.", "Asleep. It can be woken over the network.";
    DetailAsleepNoWake => "Dormindo", "Asleep";
    DetailOffline => "Fora da rede", "Not on the network";
    DetailUnpaired => "Aparece na rede, ainda não pareado", "Seen on the network, not paired yet";
    ViewOnly => "Assistir", "Watch";
    Control => "Controlar", "Control";
    Wake => "Acordar", "Wake";
    PairWithDevice => "Parear", "Pair";
    TabsLabel => "Seções do dispositivo", "Device sections";
    TabOverview => "Visão geral", "Overview";
    TabPermissions => "Permissões", "Permissions";
    TabHistory => "Histórico", "History";

    // Overview
    SectionWhoControls => "Quem controla quem", "Who controls whom";
    YouControl => "Você controla", "You control";
    SwapNote => "Durante a sessão, qualquer um dos dois pode pedir para inverter.", "During a session either of you can ask to swap.";
    PermsPanelTitle => "O {0} pode, aqui", "What {0} can do here";
    PermsPanelEdit => "Editar", "Edit";
    PermsNone => "Nada liberado aqui.", "Nothing is allowed here.";
    ListAnd => " e ", " and ";
    SumView => "ver a tela", "see the screen";
    SumControl => "controlar", "control";
    SumAudio => "ouvir o áudio", "hear the audio";
    SumTerminal => "terminal", "terminal";
    SumAdminTerminal => "terminal de administrador", "administrator terminal";
    SumFiles => "arquivos", "files";
    SumClipboard => "área de transferência", "clipboard";
    SumUnattended => "acesso sem usuário", "access with no one present";
    LastSessionTitle => "Última sessão", "Last session";
    LastSessionLine => "{0}, por {1}, {2}.", "{0}, for {1}, {2}.";
    LastSessionNone => "Ainda sem sessões com este dispositivo.", "No sessions with this device yet.";
    WhenToday => "Hoje às {0}", "Today at {0}";
    WhenYesterday => "Ontem às {0}", "Yesterday at {0}";
    WhenDate => "{0} às {1}", "{0} at {1}";
    TableToday => "Hoje, {0}", "Today, {0}";
    TableYesterday => "Ontem, {0}", "Yesterday, {0}";
    TableDate => "{0}, {1}", "{0}, {1}";
    MinutesLess => "menos de um minuto", "less than a minute";
    MinutesOne => "1 minuto", "1 minute";
    MinutesMany => "{0} minutos", "{0} minutes";
    WhereDefault => "na área de trabalho", "on the desktop";
    WhereSecure => "na tela de segurança", "on the secure screen";
    WhereLocked => "na tela bloqueada", "on the lock screen";
    WhereLogin => "na tela de login deste computador", "on this computer's sign-in screen";

    // Permissions tab
    PermsHereTitle => "O {0} pode, neste computador", "What {0} can do on this computer";
    PermsThereTitle => "Este computador pode, no {0}", "What this computer can do on {0}";
    PermView => "Ver a tela", "See the screen";
    PermControl => "Controlar mouse e teclado", "Control mouse and keyboard";
    PermAudio => "Ouvir o áudio", "Hear the audio";
    PermTerminal => "Terminal", "Terminal";
    PermAdminTerminal => "Terminal de administrador", "Administrator terminal";
    PermAdminTerminalHint => "Comandos como sistema", "Runs commands as the system";
    PermFiles => "Arquivos", "Files";
    PermClipboard => "Área de transferência", "Clipboard";
    PermUnattended => "Entrar sem perguntar", "Enter without asking";
    PermUnattendedHint => "Inclui a tela de login e o bloqueio", "Includes the sign-in and lock screens";
    PermYes => "Sim", "Yes";
    PermNo => "Não", "No";
    PermUnknown => "–", "–";
    ThereNote => "Quem decide é o {0}. Para inverter, o lado que passa a ser controlado precisa ter liberado “Controlar” para o outro.", "{0} decides. To swap, the side that becomes controlled must have allowed “Control” for the other.";
    ForgetDevice => "Esquecer este dispositivo", "Forget this device";
    RenameAction => "Renomear", "Rename";
    RenameSave => "Salvar", "Save";
    RenameLabel => "Nome do dispositivo", "Device name";
    RenameEmpty => "Digite um nome.", "Type a name.";
    RenameTooLong => "Esse nome é longo demais.", "That name is too long.";
    RenameControlChars => "O nome não pode ter caracteres de controle.", "The name cannot contain control characters.";
    HistoryTitle => "Histórico de {0}", "History of {0}";
    HistoryPlaceholder => "O histórico por dispositivo chega numa próxima versão. Por enquanto, veja tudo em Atividade.", "Per-device history is coming in a later version. For now, see everything under Activity.";

    // Pairing
    PairingBarPrefix => "Swapdesk no", "Swapdesk on";
    PairingModeFixed => "Modo de pareamento aberto por 2 minutos nos dois computadores", "Pairing mode is open for 2 minutes on both computers";
    PairingModeCountdown => "Modo de pareamento aberto, restam {0}", "Pairing mode is open, {0} left";
    PairTitleWith => "Parear com {0}", "Pair with {0}";
    PairCompare => "Confira se o código abaixo é igual ao que aparece agora no {0}.", "Check that the code below matches the one now showing on {0}.";
    PairCodeLabel => "Código {0}", "Code {0}";
    PairWarn => "Se forem diferentes, cancele: outro dispositivo pode estar tentando se passar pelo {0}.", "If they differ, cancel: another device may be trying to pose as {0}.";
    PairSame => "Os códigos são iguais", "The codes match";
    PairDifferent => "São diferentes", "They differ";
    PairOpenBody => "Abra “Parear este computador” também no outro computador antes que o tempo abaixo acabe. Pedidos de pareamento sem isso são ignorados.", "Open “Pair this computer” on the other computer too, before the time below runs out. Pairing requests without it are ignored.";
    PairTimeLeft => "Tempo para parear: {0}", "Time left to pair: {0}";
    PairNotPairedBody => "Este computador ainda não está pareado. Para parear, abra “Parear este computador” nos dois computadores, com no máximo 2 minutos entre um e outro.", "This computer is not paired yet. To pair, open “Pair this computer” on both computers, at most 2 minutes apart.";
    PairSearching => "Procurando computadores com o pareamento aberto…", "Looking for computers with pairing open…";
    PairCancel => "Cancelar", "Cancel";
    PairWaitingTitle => "Esperando o outro computador", "Waiting for the other computer";
    PairWaitingBody => "O outro lado precisa confirmar o código. Isso leva alguns segundos.", "The other side has to confirm the code. This takes a few seconds.";
    PairDoneTitle => "Pareado com {0}", "Paired with {0}";
    PairDoneBody => "Os dois computadores agora confiam um no outro. Ajuste o que cada um pode fazer na aba Permissões.", "The two computers now trust each other. Adjust what each can do in the Permissions tab.";
    PairDoneButton => "Pronto", "Done";
    PairFailedTitle => "Não foi possível parear", "Pairing failed";
    PairFailedBody => "Nada foi liberado. Abra o pareamento nos dois computadores e tente de novo.", "Nothing was allowed. Open pairing on both computers and try again.";
    PairRetry => "Tentar de novo", "Try again";
    PairClose => "Fechar", "Close";

    // Incoming request
    IncomingTitleView => "{0} quer ver este computador", "{0} wants to see this computer";
    IncomingTitleControl => "{0} quer controlar este computador", "{0} wants to control this computer";
    IncomingSub => "Recusa automática em {0} s.", "Declined automatically in {0} s.";
    AllowThisSession => "Permitir nesta sessão", "Allow for this session";
    AllowControl => "Controlar", "Control";
    AllowAudio => "Ouvir o áudio", "Hear the audio";
    AllowFiles => "Arquivos", "Files";
    AllowClipboard => "Área de transferência", "Clipboard";
    AllowTerminal => "Terminal", "Terminal";
    RememberUnattended => "Deixar entrar sem perguntar da próxima vez", "Let it in without asking next time";
    Decline => "Recusar", "Decline";
    ViewOnlyAccept => "Só deixar ver", "Allow watching only";
    Accept => "Aceitar", "Accept";
    RequestDialogLabel => "Pedido de conexão", "Connection request";

    // Consent card (D13)
    ConsentTitleTerminal => "{0} quer abrir o terminal desta conta", "{0} wants to open this account’s terminal";
    ConsentTitleFiles => "{0} quer abrir os arquivos desta conta", "{0} wants to open this account’s files";
    ConsentBody => "O acesso sem perguntar foi liberado por outro usuário. Terminal e arquivos desta conta só abrem se você disser sim. Tela e controle seguem sem perguntar.", "Another user granted “Enter without asking”. This account’s terminal and files open only if you say yes. Screen and control still go ahead without asking.";
    ConsentCountdown => "Recusa automática em {0} s", "Declined automatically in {0} s";
    ConsentAllow => "Permitir", "Allow";
    ConsentDeny => "Recusar", "Decline";

    // Elevation step
    ElevTitle => "Confirmação de administrador", "Administrator confirmation";
    ElevBodyWindows => "Esta mudança precisa de um administrador deste computador. O Windows vai pedir a sua confirmação.", "This change needs an administrator of this computer. Windows will ask you to confirm.";
    ElevBodyMac => "Esta mudança precisa de um administrador deste computador. O macOS vai pedir a senha de administrador.", "This change needs an administrator of this computer. macOS will ask for the administrator password.";
    ElevActGrantUnattended => "Liberar “Entrar sem perguntar” para {0}", "Allow “Enter without asking” for {0}";
    ElevActGrantAdminTerminal => "Liberar o terminal de administrador para {0}", "Allow the administrator terminal for {0}";
    ElevActGrants => "Mudar o que {0} pode fazer neste computador", "Change what {0} can do on this computer";
    ElevActElevateSession => "Elevar o controle em {0}", "Elevate control on {0}";
    ElevActAdminTerminalOpen => "Abrir o terminal de administrador", "Open the administrator terminal";
    ElevContinue => "Continuar", "Continue";
    ElevCancel => "Cancelar", "Cancel";
    ElevWaiting => "Esperando a confirmação do sistema…", "Waiting for the system confirmation…";
    ElevFailedTitle => "A confirmação não foi concluída", "The confirmation did not finish";
    ElevFailedBody => "Nada foi alterado.", "Nothing was changed.";
    ElevRetry => "Tentar de novo", "Try again";
    ElevClose => "Fechar", "Close";

    // Activity
    ActivityTitle => "Atividade", "Activity";
    ActivitySub => "Cada conexão com este computador. O registro fica só aqui.", "Every connection to this computer. The log stays on this computer only.";
    ExportLog => "Exportar registro", "Export log";
    AlertUnknownPair => "Um dispositivo desconhecido tentou parear {0}.", "An unknown device tried to pair with {0}.";
    AlertUnknownRest => "Os códigos não foram confirmados e nada foi liberado.", "The codes were not confirmed and nothing was allowed.";
    BlockAddress => "Bloquear {0}", "Block {0}";
    LogLabel => "Registro", "Log";
    ColWhen => "Quando", "When";
    ColDevice => "Dispositivo", "Device";
    ColWhat => "O que aconteceu", "What happened";
    ColDuration => "Duração", "Duration";
    DurNow => "agora", "now";
    DurMinutes => "{0} min", "{0} min";
    DurNone => "–", "–";
    UnknownDevice => "Desconhecido", "Unknown";
    AuditSwap => "Inverteu o controle: este computador passou a controlar o {0}", "Swapped control: this computer began to control {0}";
    AuditControlFiles => "Controlou este computador e enviou {0} {0|arquivo|arquivos}", "Controlled this computer and sent {0} {0|file|files}";
    AuditUnknownPair => "Pareamento cancelado: códigos não confirmados", "Pairing cancelled: codes not confirmed";
    AuditUnattendedLogin => "Entrou sem perguntar, na tela de login", "Entered without asking, on the sign-in screen";
    AuditViewOnly => "Só viu a tela", "Only watched the screen";
    AuditPaired => "Pareado com códigos confirmados nos dois", "Paired with the codes confirmed on both";

    // Settings
    SettingsTitle => "Ajustes", "Settings";
    FieldName => "Nome", "Name";
    FieldLanguage => "Idioma", "Language";
    LangPt => "Português (Brasil)", "Português (Brasil)";
    LangEn => "English", "English";
    FieldTheme => "Tema", "Theme";
    ThemeSystem => "Sistema", "System";
    ThemeLight => "Claro", "Light";
    ThemeDark => "Escuro", "Dark";
    OpenAtLogin => "Abrir ao ligar o computador", "Open when the computer starts";
    GroupUnattended => "Sem ninguém na frente", "When no one is at the computer";
    ServiceName => "Receber conexões", "Receive connections";
    ServiceHint => "Outros computadores podem se conectar a esta conta", "Other computers can connect to this account";
    ServiceActive => "Ativo", "On";
    ServiceInactive => "Desligado", "Off";
    LockOnEnd => "Bloquear ao terminar a sessão", "Lock when the session ends";
    KeepAwake => "Não suspender durante a sessão", "Keep awake during a session";
    GroupImage => "Imagem", "Image";
    Quality => "Qualidade", "Quality";
    QualityAuto => "Automática", "Automatic";
    QualityText => "Texto nítido", "Sharp text";
    QualityMotion => "Movimento fluido", "Smooth motion";
    ShowStats => "Mostrar latência e quadros", "Show latency and frame rate";
    GroupKeyboard => "Teclado", "Keyboard";
    SendKeys => "Enviar teclas", "Send keys";
    SendKeysHint => "Automático troca para texto quando os teclados são diferentes", "Automatic switches to text when the keyboards differ";
    KeyboardAuto => "Automático", "Automatic";
    KeyboardPhysical => "Teclas físicas", "Physical keys";
    KeyboardText => "Texto", "Text";
    SwapCtrlCmd => "Trocar Ctrl e Cmd no Mac", "Swap Ctrl and Cmd on the Mac";
    GroupShortcuts => "Atalhos do visualizador", "Viewer shortcuts";
    ReleaseKeyLabel => "Tecla que libera o teclado", "Key that releases the keyboard";
    ReleaseKeyHint => "Aperte e solte, sem outra tecla no meio. Um clique na tela volta a capturar.", "Press and release it without pressing any other key. Clicking the screen captures the keyboard again.";
    ReleaseCtrlAlt => "Ctrl+Alt", "Ctrl+Alt";
    ReleaseRightCtrl => "Ctrl direito", "Right Ctrl";
    ReleaseCtrlAltShift => "Ctrl+Alt+Shift", "Ctrl+Alt+Shift";
    ReleaseControlOption => "Control+Option", "Control+Option";
    ReleaseRightControl => "Control direito", "Right Control";
    ReleaseControlOptionShift => "Control+Option+Shift", "Control+Option+Shift";
    FullscreenMonitorLabel => "Monitor da tela cheia", "Full-screen monitor";
    FullscreenMonitorAuto => "Onde a janela está", "Where the window is";
    FullscreenMonitorN => "Monitor {0}", "Monitor {0}";
    ShortcutsTitle => "Atalhos", "Shortcuts";
    ScReleaseKeyboard => "Liberar o teclado", "Release the keyboard";
    ScShowBar => "Mostrar ou recolher a barra da sessão", "Show or collapse the session bar";
    ScFullScreen => "Tela cheia", "Full screen";
    ScCtrlAltDel => "Enviar Ctrl+Alt+Del", "Send Ctrl+Alt+Del";
    ScDisconnect => "Desconectar", "Disconnect";
    ShortcutsKeepHint => "Valem também com “Capturar atalhos do sistema” ligado.", "They also work with “Capture system shortcuts” on.";
    GroupNetwork => "Rede", "Network";
    NetworkRow => "{0}, {1}", "{0}, {1}";
    NetworkRowNoAddress => "{0}", "{0}";
    NetworkHint => "WSL, Docker e VPN ficam de fora", "WSL, Docker and VPN are left out";

    // Notices
    NoticePublicTitle => "O Windows marcou esta rede como Pública", "Windows marked this network as Public";
    NoticePublicBody => "Em rede pública ninguém consegue se conectar a este computador. Marque o {0} como rede Privada.", "On a public network nobody can connect to this computer. Mark {0} as a Private network.";
    NoticePublicButton => "Abrir configurações de rede", "Open network settings";
    NoticeFirewallTitle => "O firewall está bloqueando o Swapdesk", "The firewall is blocking Swapdesk";
    NoticeFirewallBody => "Outros computadores não conseguem encontrar este. Libere o Swapdesk para redes privadas no Firewall do Windows.", "Other computers cannot find this one. Allow Swapdesk on private networks in Windows Firewall.";
    NoticeFirewallButton => "Abrir configurações de rede", "Open network settings";
    NoticeServiceTitle => "O serviço do Swapdesk espera aprovação no Mac", "The Swapdesk service waits for approval on the Mac";
    NoticeServiceBody => "Sem ele o Mac não aparece na rede. Ligue o Swapdesk em Ajustes do Sistema, Geral, Itens de Início.", "Without it the Mac does not show on the network. Turn Swapdesk on in System Settings, General, Login Items.";
    NoticeCrashTitle => "O Swapdesk fechou inesperadamente", "Swapdesk closed unexpectedly";
    NoticeCrashBody => "O registro do que aconteceu ficou na pasta de logs. Para relatar o problema, anexe esse arquivo.", "A record of what happened is in the logs folder. To report the problem, attach that file.";
    NoticeCrashButton => "Abrir a pasta de logs", "Open logs folder";
    NoComputerTitle => "Nenhum computador encontrado", "No computer found";
    NoComputerBody => "Confira no outro computador: o Swapdesk está aberto e na mesma rede Wi-Fi ou cabo que este. Se continuar sem aparecer, o roteador pode estar isolando os dispositivos (rede de convidados ou isolamento de clientes), uma VPN pode estar ligada, ou os dois estão em redes diferentes.", "On the other computer, check that Swapdesk is open and on the same Wi-Fi or cable as this one. If it still does not show up, the router may be isolating devices (guest network or client isolation), a VPN may be on, or the two are on different networks.";
    NoComputerButton => "Adicionar por endereço", "Add by address";
    NoticeServiceButton => "Abrir Ajustes do Sistema", "Open System Settings";
    NoticeMacPermsOne => "Falta 1 permissão no Mac", "1 permission is missing on the Mac";
    NoticeMacPermsMany => "Faltam {0} permissões no Mac", "{0} permissions are missing on the Mac";
    NoticeMacPermsButton => "Conceder permissões", "Grant permissions";
    OsPermScreen => "Gravação de Tela, para ver a tela.", "Screen Recording, to see the screen.";
    OsPermAccessibility => "Acessibilidade, para mexer o mouse e o teclado.", "Accessibility, to control the mouse and keyboard.";
    OsPermInput => "Monitoramento de Entrada, para enviar atalhos como Cmd+Tab.", "Input Monitoring, to send shortcuts such as Cmd+Tab.";
    ReconnectingTitle => "Reconectando ao {0}", "Reconnecting to {0}";
    ReconnectingBody => "O Wi-Fi caiu. A tela volta sozinha quando a rede voltar. Tentativa {0}.", "The Wi-Fi dropped. The screen returns by itself when the network is back. Attempt {0}.";
    WakingTitle => "Acordando o {0}", "Waking {0}";
    WakingBody => "O sinal foi enviado. A conexão abre assim que ele aparecer na rede, em até 1 minuto.", "The signal was sent. The connection opens as soon as it shows on the network, within 1 minute.";
    SwapDeclinedTitle => "O {0} não aceitou inverter", "{0} did not accept the swap";
    SwapDeclinedBody => "Ninguém respondeu em 20 segundos. Você continua controlando o {0}.", "Nobody answered in 20 seconds. You are still controlling {0}.";
    Cancel => "Cancelar", "Cancel";
    Close => "Fechar", "Close";
    Disconnect => "Desconectar", "Disconnect";
    ErrorLabel => "Aviso", "Notice";
    ErrServiceDown => "O serviço do Swapdesk não está respondendo.", "The Swapdesk service is not responding.";
    ErrNotFound => "Dispositivo não encontrado.", "Device not found.";
    ErrNotPermitted => "Isso não foi liberado.", "That was not allowed.";
    ErrNeedsAdmin => "Isso precisa de um administrador.", "That needs an administrator.";
    ErrBusy => "Ocupado, tente de novo.", "Busy, try again.";
    ErrTimeout => "Sem resposta.", "No response.";
    ErrRefused => "Recusado.", "Declined.";
    ErrLocked => "Pareamento bloqueado por 15 minutos.", "Pairing locked for 15 minutes.";
    ErrInvalid => "Valor inválido.", "Invalid value.";
    ErrInternal => "Erro interno.", "Internal error.";

    // Viewer window
    ViewerTitle => "{0}, Swapdesk", "{0}, Swapdesk";
    ViewerControlling => "você controla há {0}", "you have been controlling for {0}";
    ViewerViewing => "você está vendo há {0}", "you have been watching for {0}";
    ViewerConnecting => "conectando…", "connecting…";
    ViewerStats => "{0} ms, {1} qps, {2}", "{0} ms, {1} fps, {2}";
    ViewerScreenOf => "Tela do {0}", "{0}’s screen";
    VideoPlaceholder => "A tela remota aparece aqui.", "The remote screen appears here.";
    ToolbarLabel => "Sessão", "Session";
    TbScreen => "Escolher tela", "Choose screen";
    TbScreenN => "Tela {0}", "Display {0}";
    TbFit => "Ajustar à janela", "Fit to window";
    TbAudio => "Áudio", "Audio";
    TbGrab => "Capturar atalhos do sistema", "Capture system shortcuts";
    TbKeys => "Enviar Ctrl+Alt+Del e outras teclas", "Send Ctrl+Alt+Del and other keys";
    TbTerminal => "Terminal", "Terminal";
    TbFiles => "Arquivos", "Files";
    TbChat => "Chat", "Chat";
    TbSwap => "Inverter", "Swap";
    TipScreen => "Escolher qual tela do outro computador aparece", "Choose which screen of the other computer is shown";
    TipFit => "Ajustar a imagem ao tamanho da janela", "Fit the picture to the window size";
    TipAudio => "Ouvir o som do outro computador", "Hear the other computer's sound";
    TipGrab => "Enviar os atalhos do sistema ao outro computador", "Send system shortcuts to the other computer";
    TipTerminal => "Abrir um terminal no outro computador", "Open a terminal on the other computer";
    TipFiles => "Enviar e receber arquivos", "Send and receive files";
    TipChat => "Conversar com quem está no outro computador", "Chat with the person at the other computer";
    TipSwap => "Inverter o controle: o outro computador passa a controlar este", "Swap control: the other computer controls this one";
    TipDisconnect => "Encerrar a sessão ({0})", "End the session ({0})";
    DrawerLabel => "Ferramentas da sessão", "Session tools";
    DrawerTabsLabel => "Ferramentas", "Tools";
    DrawerTerminal => "Terminal", "Terminal";
    DrawerFiles => "Arquivos", "Files";
    DrawerChat => "Chat", "Chat";
    DrawerClipboard => "Copiar e colar", "Copy and paste";
    ClosePanel => "Fechar painel", "Close panel";
    TerminalNote => "Comandos rodam no {0} como o usuário {1}.", "Commands run on {0} as the user {1}.";
    TermOpening => "Abrindo o terminal…", "Opening the terminal…";
    TermAdminNote => "Terminal de administrador: os comandos rodam em {0} com todos os direitos do sistema.", "Administrator terminal: commands run on {0} with full system rights.";
    TermOpenAdmin => "Abrir terminal de administrador", "Open administrator terminal";
    TermReopen => "Abrir de novo", "Open again";
    TermExitedCode => "O terminal terminou (código {0}).", "The terminal ended (code {0}).";
    TermEnded => "O terminal foi encerrado.", "The terminal was closed.";
    TermHint => "Ctrl+Shift+C copia o texto selecionado e Ctrl+V cola.", "Ctrl+Shift+C copies the selected text and Ctrl+V pastes.";
    TermFailNobody => "Ninguém está usando o computador agora, então o terminal não abre.", "Nobody is using the computer right now, so the terminal cannot open.";
    TermFailDeclined => "O usuário do computador não deixou abrir o terminal.", "The computer's user did not allow the terminal.";
    TermFailTimedOut => "O usuário do computador não respondeu a tempo.", "The computer's user did not answer in time.";
    TermFailNotPermitted => "Este dispositivo não tem permissão para usar o terminal.", "This device is not allowed to use the terminal.";
    TermFailTooMany => "Já há terminais demais abertos nesta sessão.", "There are too many terminals open in this session.";
    TermFailUnavailable => "O terminal não está disponível no computador agora.", "The terminal is not available on the computer right now.";
    TermFailed => "Não foi possível abrir o terminal.", "The terminal could not be opened.";
    QuickActions => "Ações rápidas", "Quick actions";
    ActLock => "Bloquear a tela", "Lock the screen";
    ActSleep => "Suspender", "Sleep";
    ActRestart => "Reiniciar", "Restart";
    ActTaskMonitorMac => "Monitor de Atividade", "Activity Monitor";
    ActTaskMonitorWindows => "Gerenciador de Tarefas", "Task Manager";
    FilesFolderOn => "Pasta no {0}", "Folder on {0}";
    ChangeFolder => "Trocar pasta", "Change folder";
    FilesDownload => "Baixar", "Download";
    FilesOpen => "Abrir", "Open";
    FilesItems => "{0} {0|item|itens}", "{0} {0|item|items}";
    FilesDrop => "Envie arquivos deste computador para a pasta que está aberta em {0}.", "Send files from this computer to the folder that is open on {0}.";
    ChooseFiles => "Escolher arquivos", "Choose files";
    Transfers => "Transferências", "Transfers";
    TransferProgress => "{0} de {1}, {2}/s", "{0} of {1}, {2}/s";
    TransferPaused => "Pausado: a conexão caiu", "Paused: the connection dropped";
    TransferResume => "Retomar", "Resume";
    TransferSent => "Enviado para {0}", "Sent to {0}";
    TransferReceived => "Recebido", "Received";
    TransferFailed => "Não terminou", "Did not finish";
    FilesUp => "Pasta acima", "Parent folder";
    FilesPickFolder => "Escolher a pasta para receber os arquivos", "Choose the folder to receive files into";
    FilesPrevPage => "Anteriores", "Previous";
    FilesNextPage => "Mais itens", "More items";
    FilesSaveTo => "Salvar em {0}", "Save to {0}";
    FilesEmpty => "Esta pasta está vazia.", "This folder is empty.";
    FilesTruncated => "Só os primeiros itens aparecem: a pasta tem mais do que cabe na lista.", "Only the first items are shown: the folder has more than the list holds.";
    TransferSavedAs => "Já existia um arquivo com esse nome: salvo como {0}", "A file with that name already existed: saved as {0}";
    TransferRefused => "Recusado: o outro computador não permitiu", "Refused: the other computer did not allow it";
    ChatNote => "As mensagens somem quando a sessão termina.", "Messages disappear when the session ends.";
    ChatPlaceholder => "Escreva uma mensagem", "Write a message";
    ChatLabel => "Mensagem", "Message";
    ChatSendButton => "Enviar", "Send";
    ChatReadAt => "Lida às {0}", "Read at {0}";
    ChatSwapped => "Controle invertido às {0}", "Control swapped at {0}";
    ClipTitle => "Copiar e colar entre os dois", "Copy and paste between the computers";
    ClipBody => "Texto e imagens. Senhas copiadas de gerenciadores ficam de fora.", "Text and images. Passwords copied from password managers are left out.";

    // Host banner and swap
    BannerControls => "{0} controla este computador", "{0} controls this computer";
    BannerViews => "{0} está vendo este computador", "{0} is watching this computer";
    BannerSince => "há {0}", "for {0}";
    BannerOpenChat => "Abrir chat", "Open chat";
    BannerLabel => "Sessão ativa", "Active session";
    SwapConfirmTitle => "Inverter o controle?", "Swap control?";
    SwapConfirmBody => "O {0} passa a controlar este computador e você deixa de controlar o {0}. Quem está no {0} precisa aceitar.", "{0} will control this computer and you will stop controlling {0}. Whoever is at {0} has to accept.";
    SwapAskButton => "Pedir inversão", "Ask to swap";
    SwapAskTitle => "O {0} pediu para inverter", "{0} asked to swap";
    SwapAskBody => "Se aceitar, você passa a controlar o {0} e ele deixa de controlar este computador.", "If you accept, you will control {0} and it will stop controlling this computer.";
    SwapAskCountdown => "Recusa automática em {0} s", "Declined automatically in {0} s";
    SwapAccept => "Aceitar e controlar", "Accept and control";
    SwapDecline => "Recusar", "Decline";

    // Stage 41: swap reasons, chat badge, wake and reconnect endings. New keys go last: a key travels as its index.
    SwapNotNowTitle => "Não dá para inverter agora", "Cannot swap right now";
    SwapWhyDeclined => "O {0} recusou.", "{0} declined.";
    SwapWhyNobody => "Não há ninguém no {0} agora.", "Nobody is at {0} right now.";
    SwapWhyNotPermittedHere => "Você ainda não deu ao {0} a permissão de controlar este computador. Dê em Permissões e tente de novo.", "You have not given {0} permission to control this computer. Grant it in Permissions and try again.";
    SwapWhyNotPermittedThere => "O {0} ainda não deu a você a permissão de controlar o computador dele.", "{0} has not given you permission to control their computer.";
    SwapWhyBusy => "Já existe um pedido de inversão em andamento.", "A swap request is already under way.";
    SwapStays => "Você continua controlando o {0}.", "You are still controlling {0}.";
    TbChatUnread => "Chat, {0} não lidas", "Chat, {0} unread";
    BannerChatUnread => "Abrir chat, {0} não lidas", "Open chat, {0} unread";
    ReconnectGaveUp => "Não deu para reconectar ao {0}. A sessão terminou.", "Could not reconnect to {0}. The session ended.";
    WakeGaveUp => "O {0} não apareceu na rede em 1 minuto.", "{0} did not show on the network within 1 minute.";

    // The connect flow's line on the main window (appended after the first release of the table)
    PromptedTitle => "Esperando o {0} aceitar", "Waiting for {0} to accept";
    ConnectingTitle => "Conectando ao {0}", "Connecting to {0}";
    // Unused since the connect card says why it ended: it stays because a key travels as its index.
    EndedTitle =>"A sessão com o {0} terminou", "The session with {0} ended";

    // The macOS application menu
    MenuQuit => "Encerrar o Swapdesk", "Quit Swapdesk";
    MenuEdit => "Editar", "Edit";
    MenuCut => "Recortar", "Cut";
    MenuCopy => "Copiar", "Copy";
    MenuPaste => "Colar", "Paste";
    MenuSelectAll => "Selecionar tudo", "Select all";

    // The macOS menu bar status item (D32: the app keeps hosting with its window closed)
    StatusOpen => "Abrir o Swapdesk", "Open Swapdesk";
    StatusQuit => "Sair do Swapdesk", "Quit Swapdesk";

    // The "send keys" chords, written the same in both languages
    KeysCtrlAltDel => "Ctrl+Alt+Del", "Ctrl+Alt+Del";
    KeysAltTab => "Alt+Tab", "Alt+Tab";
    KeysCmdTab => "Cmd+Tab", "Cmd+Tab";
    KeysWindows => "Windows", "Windows";
    KeysCmd => "Cmd", "Cmd";
    KeysAltF4 => "Alt+F4", "Alt+F4";
    KeysCmdQ => "Cmd+Q", "Cmd+Q";
    KeysPrintScreen => "Print Screen", "Print Screen";

    // Clipboard and quick actions (stage 42)
    ClipSkippedTooLarge => "O que foi copiado é grande demais e não foi enviado.", "What was copied is too large and was not sent.";
    ClipSkippedConcealed => "O que foi copiado veio marcado como privado, como uma senha, e não foi enviado.", "What was copied was marked as private, like a password, and was not sent.";
    ClipSkippedUnsupported => "Esse tipo de conteúdo não pode ser copiado entre os computadores.", "This kind of content cannot be copied between the computers.";
    ActionFailed => "{0} não executou a ação.", "{0} did not run the action.";

    // Activity lines of a control session, and of a device no longer paired (after 1.0)
    AuditControlled => "Controlou este computador", "Controlled this computer";
    AuditControlledFromHere => "Controlado a partir deste computador", "Controlled from this computer";
    RemovedDevice => "Dispositivo removido", "Removed device";
    RemovedDeviceInLine => "dispositivo removido", "removed device";

    // Elevation step and window. Appended last: a key travels as its index, so a new key in the middle would shift
    // the activity texts of a service built before it.
    ElevNotAdmin => "A confirmação não veio de um administrador deste computador.", "The confirmation did not come from an administrator of this computer.";
    ElevTimedOut => "O tempo para confirmar acabou.", "The time to confirm ran out.";
    ElevConfirmBody => "Confirme como administrador para aplicar esta mudança.", "Confirm as an administrator to apply this change.";
    ElevConfirm => "Confirmar", "Confirm";

    // Waits, appended last as above. `EndedTitle` stays unused further up: a key travels as its index.
    ConnectingBody => "Abrindo a conexão pela rede local.", "Opening the connection over the local network.";
    PromptedBody => "O {0} está mostrando o seu pedido. Quem estiver nele precisa aceitar e escolher o que você pode fazer.", "{0} is showing your request. Someone there has to accept it and choose what you may do.";
    ConnectExpiresIn => "O pedido expira em {0} s", "The request expires in {0} s";
    ConnectEndedTitle => "Não deu para conectar ao {0}", "Could not connect to {0}";
    ConnectWhyTimedOut => "O {0} não respondeu em 20 segundos.", "{0} did not answer within 20 seconds.";
    ConnectWhyBusy => "O {0} já está em outra sessão. Tente de novo quando ela terminar.", "{0} is already in another session. Try again when it ends.";
    ConnectWhyForgotten => "O {0} não reconhece mais este computador. Pareie os dois de novo.", "{0} no longer knows this computer. Pair the two again.";
    ConnectWhyNotPermitted => "O {0} não permite esta conexão.", "{0} does not allow this connection.";
    ConnectWhyUnreachable => "O {0} não respondeu pela rede. Confira se ele está ligado e nesta mesma rede.", "{0} did not answer over the network. Check that it is on and on this same network.";
    SidebarConnecting => "Conectando…", "Connecting…";
    SidebarWaitingAccept => "Esperando aceitar", "Waiting to be accepted";
    SidebarWaking => "Acordando…", "Waking…";
    SwapWaitingTitle => "Esperando o {0} aceitar a inversão", "Waiting for {0} to accept the swap";
    SwapWaitingBody => "Se ninguém responder em 20 segundos, você continua controlando o {0}.", "If nobody answers within 20 seconds, you keep controlling {0}.";
    TermConsentWaiting => "Esperando quem usa o {0} permitir o terminal.", "Waiting for the person using {0} to allow the terminal.";
    FilesConsentWaiting => "Esperando quem usa o {0} permitir os arquivos.", "Waiting for the person using {0} to allow file access.";
    ActionWaiting => "Esperando o {0} executar a ação…", "Waiting for {0} to run the action…";
    PermAdminTerminalMacOff => "Indisponível no macOS nesta versão", "Not available on macOS in this version";
    SidebarLegacyNote => "Os pareamentos de antes da versão 1.1 não passam para as contas. Pareie de novo com os computadores que você usa.", "Pairings made before version 1.1 do not carry over to accounts. Pair again with the computers you use.";
    AuditElevated => "Controle elevado", "Elevated control";
    ElevateButton => "Elevar controle", "Elevate control";
    ElevateHint => "Controlar também os avisos de administrador e a tela de entrada", "Also control administrator prompts and the sign-in screen";
    PausedAwayBody => "Esta conta não está mais na tela do {0}. A sessão volta quando ela voltar.", "This account is no longer on {0}'s screen. The session resumes when it is back.";
    PausedElevateBody => "O {0} mostra uma tela protegida. Eleve o controle para vê-la e usá-la.", "{0} shows a protected screen. Elevate control to see and use it.";
    NameNetworkHint => "Outros computadores da rede veem este nome.", "Other computers on the network see this name.";
    ElevDeclined => "A confirmação foi recusada.", "The confirmation was declined.";
    ElevBodyWindowsCredentials => "Esta mudança precisa de um administrador deste computador. O Windows vai pedir o nome e a senha de um administrador.", "This change needs an administrator of this computer. Windows will ask for an administrator's name and password.";

    // Updates (UPDATES.md 10.4, approved by the owner 2026-11-11, D58). New keys go at the end: the wire carries a
    // key's index.
    UpdTitle => "Nova versão: {0}", "New version: {0}";
    UpdBody => "Você usa a {0}. A atualização vem do GitHub, é conferida pela assinatura do Swapdesk e instalada; o Swapdesk fecha e abre de novo.", "You have {0}. The update is downloaded from GitHub, verified against the Swapdesk signature and installed. Swapdesk closes and reopens.";
    UpdInstall => "Atualizar agora", "Update now";
    UpdLater => "Agora não", "Not now";
    UpdNotes => "O que mudou", "What changed";
    UpdLiveBody => "Há uma sessão ativa com {0}. Atualizar agora encerra a sessão.", "A session with {0} is active. Updating now ends it.";
    UpdLiveRemote => "Se você controla este computador de longe, conecte-se de novo em um minuto. Sem “Entrar sem perguntar”, alguém aqui terá de aceitar.", "If you control this computer from elsewhere, reconnect in a minute. Without “Enter without asking”, someone here has to accept.";
    UpdLiveConfirm => "Encerrar e atualizar", "End and update";
    UpdDownloading => "Baixando {0} de {1}", "Downloading {0} of {1}";
    UpdVerifying => "Conferindo a assinatura", "Checking the signature";
    UpdInstalling => "Instalando. O Swapdesk vai fechar e abrir de novo.", "Installing. Swapdesk will close and open again.";
    UpdDone => "Swapdesk atualizado para {0}.", "Swapdesk was updated to {0}.";
    UpdErrNetwork => "Não foi possível falar com o GitHub. Confira a conexão com a internet.", "Could not reach GitHub. Check the internet connection.";
    UpdErrWait => "O GitHub pediu para esperar. Tente mais tarde.", "GitHub asked to wait. Try again later.";
    UpdErrSignature => "O que foi baixado não tem a assinatura do Swapdesk. Foi apagado e nada foi instalado.", "What was downloaded does not carry the Swapdesk signature. It was deleted and nothing was installed.";
    UpdErrMismatch => "O arquivo baixado não confere com a versão anunciada. Foi apagado e nada foi instalado.", "The downloaded file does not match the announced version. It was deleted and nothing was installed.";
    UpdErrDisk => "Falta espaço em disco: a atualização precisa de {0}.", "Not enough disk space: the update needs {0}.";
    UpdErrRolledBack => "A versão {0} não iniciou. A {1} foi restaurada e continua funcionando.", "Version {0} did not start. {1} was restored and keeps working.";
    UpdErrMacAdmin => "Só um administrador deste Mac pode substituir o Swapdesk em Aplicativos.", "Only an administrator of this Mac can replace Swapdesk in Applications.";
    UpdErrMacLocation => "Mova o Swapdesk para a pasta Aplicativos para que ele possa se atualizar.", "Move Swapdesk to the Applications folder so it can update itself.";
    UpdManual => "Esta versão precisa ser instalada à mão.", "This version must be installed manually.";
    UpdSection => "Atualizações", "Updates";
    UpdVersion => "Versão {0}", "Version {0}";
    UpdSwitch => "Procurar novas versões no GitHub", "Check GitHub for new versions";
    UpdSwitchHint => "Uma vez por dia este computador pergunta ao GitHub qual é a versão mais recente. Nada sobre você ou este computador é enviado.", "Once a day this computer asks GitHub which version is the latest. Nothing about you or this computer is sent.";
    UpdCheckNow => "Procurar agora", "Check now";
    UpdLast => "Última procura: {0}", "Last check: {0}";
    UpdLatest => "Você tem a versão mais recente.", "You have the latest version.";
    UpdRetry => "Tentar de novo", "Try again";
    UpdTryLater => "Tentar mais tarde", "Try again later";
    UpdOpenRelease => "Abrir a página da versão", "Open the release page";
    UpdDownloadByHand => "Baixar à mão", "Download manually";
    UpdClose => "Fechar", "Close";
    UpdCancel => "Cancelar", "Cancel";
    // The files explorer (docs/FILES-UI.md section 9; texts for the owner's approval in R1)
    PausedMacLocked => "O {0} está bloqueado e ninguém consegue desbloqueá-lo pela rede. A sessão continua quando o Mac for desbloqueado.", "{0} is locked, and nobody can unlock it remotely. The session continues when the Mac is unlocked.";
    DisplayNumbered => "Tela {0}", "Display {0}";
    DisplayOption => "{0}, {1}", "{0}, {1}";
    DisplayOptionPrimary => "{0} (principal), {1}", "{0} (main), {1}";
    GuideTitle => "Bem-vindo ao Swapdesk", "Welcome to Swapdesk";
    GuideBody => "1. Instale o Swapdesk no outro computador, na mesma rede.\n2. Escolha-o em Na rede e clique em Parear; confira se o código é o mesmo nos dois.\n3. Em Permissões, diga o que ele pode fazer aqui. Depois é só clicar em Controlar.", "1. Install Swapdesk on the other computer, on the same network.\n2. Select it under “On the network” and click Pair. Check that both computers show the same code.\n3. In Permissions, choose what it may do here. Then click Control.";
    GuideSkip => "Pular", "Skip";
    GuideDone => "Entendi", "Got it";
    ExplorerTitle => "Arquivos", "Files";
    ExplorerBackToScreen => "Tela", "Screen";
    ExplorerThisComputer => "Este computador", "This computer";
    ExplorerCopyTo => "Copiar para {0}", "Copy to {0}";
    ExplorerCopyHere => "Copiar para este computador", "Copy to this computer";
    ExplorerOpen => "Abrir", "Open";
    ExplorerRefresh => "Atualizar", "Refresh";
    ExplorerBack => "Voltar", "Back";
    ExplorerForward => "Avançar", "Forward";
    ExplorerUp => "Pasta acima", "Up";
    ExplorerColName => "Nome", "Name";
    ExplorerColSize => "Tamanho", "Size";
    ExplorerColType => "Tipo", "Type";
    ExplorerColModified => "Modificado", "Modified";
    ExplorerItems => "{0} {0|item|itens}", "{0} {0|item|items}";
    ExplorerOneItem => "1 item", "1 item";
    ExplorerSelected => "{0} {0|selecionado|selecionados}, {1}", "{0} selected, {1}";
    ExplorerShowingFirst => "Mostrando os primeiros {0}", "Showing the first {0}";
    ExplorerEmpty => "Esta pasta está vazia.", "This folder is empty.";
    ExplorerPlaceHome => "Início", "Home";
    ExplorerPlacePersonal => "Pasta pessoal", "Home folder";
    ExplorerPlaceDesktop => "Área de trabalho", "Desktop";
    ExplorerPlaceDocuments => "Documentos", "Documents";
    ExplorerPlaceDownloads => "Downloads", "Downloads";
    ExplorerPlaceReceived => "Recebidos do Swapdesk", "Received from Swapdesk";
    ExplorerDriveLocal => "Disco local ({0})", "Local disk ({0})";
    ExplorerTypeFolder => "Pasta", "Folder";
    ExplorerTypeFile => "Arquivo", "File";
    ExplorerTypeExt => "Arquivo {0}", "{0} file";
    ExplorerTypePdf => "Documento PDF", "PDF document";
    ExplorerTypeImage => "Imagem {0}", "{0} image";
    ExplorerTypeVideo => "Vídeo {0}", "{0} video";
    ExplorerTypeAudio => "Áudio {0}", "{0} audio";
    ExplorerTypeArchive => "Arquivo compactado {0}", "{0} archive";
    ExplorerTypeText => "Documento de texto", "Text document";
    ExplorerTypeApp => "Aplicativo", "Application";
    ExplorerErrTitle => "Não foi possível abrir esta pasta", "This folder could not be opened";
    ExplorerErrGone => "Ela não existe mais. Pode ter sido movida ou apagada.", "It no longer exists. It may have been moved or deleted.";
    ExplorerErrDenied => "O {0} não deixou ler esta pasta.", "{0} did not allow reading this folder.";
    ExplorerErrDeniedHere => "Este computador não deixou ler esta pasta.", "This computer did not allow reading this folder.";
    ExplorerErrTimeout => "O {0} não respondeu a tempo.", "{0} did not answer in time.";
    ExplorerErrNoGrant => "O {0} não deu a este computador permissão para arquivos.", "{0} has not given this computer permission to access files.";
    ExplorerRetry => "Tentar de novo", "Try again";
    ExplorerGoBack => "Voltar", "Go back";
    ExplorerOpenThere => "Para abrir no {0}, use a tela.", "To open it on {0}, use the screen.";
    ExplorerWatchOnly => "Arquivos precisa de uma sessão com controle.", "Files needs a session with control.";
    ExplorerFoldersLater => "Pastas ainda não são enviadas: escolha os arquivos de dentro delas.", "Folders are not sent yet: choose the files inside them.";
    ExplorerTransfers => "Transferências", "Transfers";
    ExplorerTransfersRunning => "{0} em andamento", "{0} in progress";
    ExplorerShow => "Mostrar", "Show";
    ExplorerHide => "Ocultar", "Hide";
    // A chat message as a screen reader reads it (B17, F-UI-01).
    ChatLineMine => "Você: {0}", "You: {0}";
    ChatLinePeer => "{0}: {1}", "{0}: {1}";
    // The session bar made small and brought back (B22, owner 12/11).
    TbCollapse => "Recolher a barra", "Collapse the bar";
    TipCollapse => "Recolher a barra num puxador no topo ({0} a traz de volta)", "Shrink the bar to a handle at the top ({0} brings it back)";
    TbExpand => "Mostrar a barra da sessão ({0})", "Show the session bar ({0})";
    // Devices blocked by their MAC (B19, D65).
    AlertBlockDevice => "Bloquear este dispositivo", "Block this device";
    GroupBlocked => "Dispositivos bloqueados", "Blocked devices";
    BlockedNone => "Nenhum dispositivo bloqueado.", "No blocked devices.";
    BlockedUnnamed => "Dispositivo sem nome", "Unnamed device";
    BlockedLine => "{0} · último endereço {1} · bloqueado em {2}", "{0} · last address {1} · blocked {2}";
    Unblock => "Desbloquear", "Unblock";
    BlockedNote => "O bloqueio usa o endereço físico (MAC) do dispositivo. Um dispositivo com endereço Wi-Fi privado ou rotativo pode voltar com outro.", "Blocking uses the device’s physical (MAC) address. A device with a private or rotating Wi-Fi address can come back with another one.";
    // Slice 2a: managing this computer's files in the explorer.
    ExplorerNewFolder => "Nova pasta", "New folder";
    ExplorerRename => "Renomear", "Rename";
    ExplorerDelete => "Excluir", "Delete";
    ExplorerProps => "Propriedades", "Properties";
    ExplorerShowHidden => "Mostrar itens ocultos", "Show hidden items";
    ExplorerHideHidden => "Ocultar itens ocultos", "Hide hidden items";
    ExplorerNameTaken => "Já existe um item com esse nome aqui.", "An item with that name is already here.";
    ExplorerNameInvalid => "Esse nome não vale: sem / \\ :, sem espaço no começo nem ponto no fim, e não um nome reservado.", "That name is not allowed: no / \\ :, no leading space or trailing dot, and not a reserved name.";
    ExplorerForeverTitle => "Excluir {0} {0|item|itens} para sempre?", "Delete {0} {0|item|items} for good?";
    ExplorerForeverBody => "Isso não pode ser desfeito.", "This cannot be undone.";
    ExplorerForeverButton => "Excluir para sempre", "Delete for good";
    ExplorerNoBin => "Este local não tem Lixeira; nada foi excluído.", "This place has no Recycle Bin; nothing was deleted.";
    ExplorerMoveIntoItself => "Uma pasta não pode ir para dentro dela mesma.", "A folder cannot go inside itself.";
    ExplorerNotPermitted => "Sem permissão para mudar esse item.", "No permission to change that item.";
    ExplorerGone => "O item não está mais aí.", "The item is no longer there.";
    ExplorerOpFailed => "Não deu para concluir. Nada foi mudado.", "That could not be done. Nothing was changed.";
    ExplorerPropsTitle => "Propriedades de {0}", "Properties of {0}";
    ExplorerPropsCounting => "Contando…", "Counting…";
    ExplorerPropsContents => "{0} {0|arquivo|arquivos} e {1} {1|pasta|pastas}, {2}", "{0} {0|file|files} and {1} {1|folder|folders}, {2}";
    ExplorerPropsMore => "A contagem parou em {0} itens.", "The count stopped at {0} items.";
    ExplorerPropsFree => "Livre neste disco: {0}", "Free on this disk: {0}";
    // Slice 2b: the other computer's volumes and changes (FILES-UI.md 9)
    ExplorerTrashThereTitle => "Mandar {0} {0|item|itens} para a Lixeira do {1}?", "Move {0} {0|item|items} to the Trash on {1}?";
    ExplorerTrashThereTitleWindows => "Mandar {0} {0|item|itens} para a Lixeira do {1}?", "Move {0} {0|item|items} to the Recycle Bin on {1}?";
    ExplorerTrashThereBody => "Eles podem ser recuperados na Lixeira do {0}.", "They can be restored from the bin on {0}.";
    ExplorerNeeds11 => "O {0} usa o Swapdesk 1.0. Atualize o Swapdesk de lá para ver outros discos, criar, renomear, mover, apagar ou enviar pastas.", "{0} runs Swapdesk 1.0. Update Swapdesk there to see other disks, create, rename, move, delete or send folders.";
    ExplorerHomeOnly10 => "O {0} usa o Swapdesk 1.0: só a pasta pessoal aparece.", "{0} runs Swapdesk 1.0: only the home folder is shown.";
    ExplorerNeedsControl => "Mudar arquivos no {0} precisa também da permissão de controlar.", "Changing files on {0} also needs permission to control.";
    ExplorerErrVolumeGone => "O disco {0} foi removido do {1}.", "The disk {0} was removed from {1}.";
    ExplorerSystemProtected => "O Swapdesk não muda arquivos do sistema do {0}.", "Swapdesk does not change system files on {0}.";
    ExplorerDriveLabeled => "{0} ({1})", "{0} ({1})";
    ExplorerDriveRemovable => "Unidade removível ({0})", "Removable drive ({0})";
    ExplorerDriveNetwork => "Unidade de rede ({0})", "Network drive ({0})";
    ExplorerDriveOptical => "Unidade de CD/DVD ({0})", "CD/DVD drive ({0})";
    ExplorerVolumeUnnamed => "Volume sem nome", "Untitled volume";
    ExplorerVolumeSpace => "{0} livres de {1}", "{0} free of {1}";
    ExplorerPlacePictures => "Imagens", "Pictures";
    ExplorerPlaceMusic => "Música", "Music";
    ExplorerPlaceVideos => "Vídeos", "Videos";
    ExplorerConflictTitle => "{0} itens já existem em {1}", "{0} items already exist in {1}";
    ExplorerConflictOneTitle => "“{0}” já existe em {1}", "“{0}” already exists in {1}";
    ExplorerConflictApplyAll => "Fazer o mesmo com os outros", "Do the same for the others";
    ExplorerConflictSkip => "Pular", "Skip";
    ExplorerConflictReplace => "Substituir", "Replace";
    ExplorerConflictKeepBoth => "Manter os dois", "Keep both";
    ExplorerConflictReplaceNote => "O que for substituído vai para a Lixeira.", "What is replaced goes to the bin.";
    BannerFilesSending => "Copiando arquivos: {0} de {1}", "Copying files: {0} of {1}";
    BannerFilesReceiving => "Recebendo arquivos: {0} de {1}", "Receiving files: {0} of {1}";
    BannerFilesRenamed => "Renomeou {0} {0|item|itens}", "Renamed {0} {0|item|items}";
    BannerFilesMoved => "Moveu {0} {0|item|itens}", "Moved {0} {0|item|items}";
    BannerFilesTrashed => "Mandou {0} {0|item|itens} para a Lixeira", "Moved {0} {0|item|items} to the bin";
    BannerFilesNewFolder => "Criou uma pasta", "Created a folder";
    BannerFilesOtherDisks => "Abrindo arquivos de outros discos", "Opening files on other disks";
    AuditFilesChanged => "Mudou {0} {0|arquivo|arquivos} neste computador", "Changed {0} {0|file|files} on this computer";
    AuditFilesReach => "Abriu arquivos fora da pasta pessoal", "Opened files outside the home folder";
    // Slice 3: drag and drop inside the explorer, and the context menu (FILES-UI.md 9)
    ExplorerDragMove => "Mover para {0}", "Move to {0}";
    ExplorerDragCopy => "Copiar para {0}", "Copy to {0}";
    ExplorerDragCopyThere => "Copiar para {0}, no {1}", "Copy to {0}, on {1}";
    ExplorerDragItems => "{0} itens", "{0} items";
    ExplorerDragNoCopyHere => "Para copiar, solte no outro painel.", "To copy, drop on the other pane.";
    ExplorerCopy => "Copiar", "Copy";
    ExplorerCut => "Recortar", "Cut";
    ExplorerPaste => "Colar", "Paste";
    ExplorerRevealWindows => "Mostrar no Explorador de Arquivos", "Show in File Explorer";
    ExplorerRevealMac => "Mostrar no Finder", "Show in Finder";
    // Slices 4 and 5: drags with the system's file manager (FILES-UI.md 4.2, 4.3)
    ExplorerDiskOnly => "Só dá para enviar arquivos que estão no disco.", "Only files that are on a disk can be sent.";
    ExplorerDropOnScreen => "Solte para enviar ao {0}", "Drop to send to {0}";
    // T1: limits of one copy (FILES-UI.md 5) and folders dragged out of the window (1.1)
    ExplorerJobTooBig => "Esta cópia passa do limite de {0} arquivos ou {1} níveis de pastas. Nada foi copiado.", "This copy is over the limit of {0} files or {1} folder levels. Nothing was copied.";
    ExplorerFoldersStayIn => "Pastas ainda não podem ser arrastadas para fora do app.", "Folders cannot be dragged out of the app yet.";
    ExplorerScanning => "Lendo as pastas do {0}…", "Reading the folders on {0}…";
    ExplorerFolderCopyBusy => "Espere a cópia de pastas em andamento terminar.", "Wait for the folder copy under way to finish.";
    ExplorerPropsKind => "Tipo: {0}", "Type: {0}";
    ExplorerPropsSize => "Tamanho: {0}", "Size: {0}";
    ExplorerPropsModified => "Modificado: {0}", "Modified: {0}";
}

/// The text of `msg` in `lang`, with `{n}` replaced by the arguments (a missing argument becomes empty) and
/// `{n|one|other}` by the form the count in argument `n` takes in `lang` ([`is_one`]).
pub fn text(lang: Language, msg: &Msg) -> String {
    fill(lang, msg.key.template(lang), &msg.args)
}

/// Whether a count takes the singular form, by the CLDR cardinal rules (unicode.org/cldr/charts/latest/supplemental/
/// language_plural_rules.html, read 12/11/2026): Portuguese `one` is the integers 0 and 1, English `one` is 1. A
/// count that is not a whole number (an empty or missing argument) takes the plural.
pub fn is_one(lang: Language, count: &str) -> bool {
    let digits: String = count.chars().filter(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() != count.trim().chars().filter(|c| !matches!(c, '.' | ',')).count() {
        return false;
    }
    match digits.trim_start_matches('0') {
        "" => lang == Language::PtBr,
        "1" => true,
        _ => false,
    }
}

/// Shorthand for a key without arguments.
pub fn plain(lang: Language, key: MsgKey) -> String {
    key.template(lang).to_owned()
}

fn fill(lang: Language, template: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open + 1..];
        if let Some(close) = tail.find('}') {
            let inner = &tail[..close];
            let mut parts = inner.split('|');
            let index = parts.next().and_then(|n| n.parse::<usize>().ok());
            if let Some(n) = index {
                let arg = args.get(n).map_or("", String::as_str);
                match (parts.next(), parts.next()) {
                    (Some(one), Some(other)) => out.push_str(if is_one(lang, arg) { one } else { other }),
                    _ => out.push_str(arg),
                }
                rest = &tail[close + 1..];
                continue;
            }
        }
        out.push('{');
        rest = tail;
    }
    out.push_str(rest);
    out
}

/// Number of distinct `{n}` placeholders a template uses (the highest index plus one).
pub fn placeholders(template: &str) -> usize {
    let mut max = 0;
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let tail = &rest[open + 1..];
        if let Some(close) = tail.find('}')
            && let Ok(n) = tail[..close].split('|').next().unwrap_or_default().parse::<usize>()
        {
            max = max.max(n + 1);
            rest = &tail[close + 1..];
        } else {
            rest = tail;
        }
    }
    max
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_exists_in_both_languages_and_is_not_empty() {
        for key in MsgKey::ALL {
            for lang in [Language::PtBr, Language::En] {
                assert!(!key.template(lang).is_empty(), "{key:?} empty in {lang:?}");
            }
        }
        assert!(MsgKey::ALL.len() > 200);
    }

    #[test]
    fn both_languages_use_the_same_placeholders() {
        for key in MsgKey::ALL {
            assert_eq!(
                placeholders(key.template(Language::PtBr)),
                placeholders(key.template(Language::En)),
                "{key:?} has different arguments per language"
            );
        }
    }

    /// F-UI-01: a count chooses its noun by the language's rule; Portuguese says "0 item" and "1 item", English
    /// "0 items" and "1 item" (CLDR).
    #[test]
    fn a_count_picks_the_singular_or_the_plural_of_its_language() {
        let items = |lang, n: &str| text(lang, &Msg::new(MsgKey::ExplorerItems).with(n));
        assert_eq!(
            ["0", "1", "2", "1.000"].map(|n| items(Language::PtBr, n)),
            ["0 item", "1 item", "2 itens", "1.000 itens"]
        );
        assert_eq!(
            ["0", "1", "2", "1,000"].map(|n| items(Language::En, n)),
            ["0 items", "1 item", "2 items", "1,000 items"]
        );
        let sent = text(Language::PtBr, &Msg::new(MsgKey::AuditControlFiles).with("1"));
        assert_eq!(sent, "Controlou este computador e enviou 1 arquivo");
        // Not a whole number: the plural, never a panic.
        assert_eq!(items(Language::En, ""), " items");
        assert!(!is_one(Language::PtBr, "1,5") && !is_one(Language::En, "abc"));
    }

    /// docs/GLOSSARY.md: one word per idea; the variants it lists under "Not" never appear in a text.
    #[test]
    fn the_texts_keep_to_the_glossary() {
        let not_pt = [
            "aparelho",
            "máquina",
            " pc ",
            "visualizar",
            "acessar remotamente",
            "trocar papéis",
            "emparelh",
            "preferências",
            "acesso não assistido",
            "wake-on-lan",
        ];
        let not_en = [
            "machine",
            " pc ",
            "remote into",
            "view only",
            "switch roles",
            "preferences",
            "pasteboard",
            "unattended",
            "wake-on-lan",
        ];
        for key in MsgKey::ALL {
            // Padded, so a word at either end matches " pc " too.
            let pt = format!(" {} ", key.template(Language::PtBr).to_lowercase());
            let en = format!(" {} ", key.template(Language::En).to_lowercase());
            for w in not_pt {
                assert!(!pt.contains(w), "{key:?} (pt-BR) says {w:?}: {pt}");
            }
            for w in not_en {
                assert!(!en.contains(w), "{key:?} (en) says {w:?}: {en}");
            }
        }
    }

    #[test]
    fn arguments_are_replaced_and_missing_ones_are_empty() {
        let msg = Msg::new(MsgKey::NetworkLine).with("Wi-Fi Casa").with("192.168.0.12");
        assert_eq!(text(Language::PtBr, &msg), "Só na rede local: Wi-Fi Casa, 192.168.0.12");
        assert_eq!(text(Language::En, &msg), "Local network only: Wi-Fi Casa, 192.168.0.12");
        assert_eq!(text(Language::PtBr, &Msg::new(MsgKey::NetworkLine)), "Só na rede local: , ");
    }

    #[test]
    fn design_texts_match_the_boards() {
        assert_eq!(plain(Language::PtBr, MsgKey::ReadyToReceive), "Pronto para receber");
        assert_eq!(plain(Language::PtBr, MsgKey::PairSame), "Os códigos são iguais");
        assert_eq!(plain(Language::PtBr, MsgKey::TbGrab), "Capturar atalhos do sistema");
        assert_eq!(
            plain(Language::PtBr, MsgKey::SwapNote),
            "Durante a sessão, qualquer um dos dois pode pedir para inverter."
        );
    }

    #[test]
    fn the_status_item_menu_is_named_in_both_languages() {
        assert_eq!(plain(Language::PtBr, MsgKey::StatusOpen), "Abrir o Swapdesk");
        assert_eq!(plain(Language::En, MsgKey::StatusOpen), "Open Swapdesk");
        assert_eq!(plain(Language::PtBr, MsgKey::StatusQuit), "Sair do Swapdesk");
        assert_eq!(plain(Language::En, MsgKey::StatusQuit), "Quit Swapdesk");
    }
}
