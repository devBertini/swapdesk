# Glossary

The terms of Swapdesk's interface, one per idea, in both languages. Names the systems give their own parts (Monitor de Atividade, Monitoramento de Entrada, Configurações do Windows) are written as the system writes them. A string in `crates/swapdesk-ui/src/strings.rs` uses these words and never their variants. `strings::tests::the_texts_keep_to_the_glossary` refuses every plain variant under "Not"; a variant with a context in parentheses depends on the sentence and is checked when a text is reviewed.

| Idea | pt-BR | en | Not |
|---|---|---|---|
| An entry of the list: a computer that runs Swapdesk on this network | dispositivo (the list: Dispositivos) | device (the list: Devices) | aparelho, máquina; machine |
| This machine, or the other one, when the sentence is about the computer itself | computador ("este computador", "o outro computador") | computer ("this computer") | máquina, PC; machine, PC |
| What the other computer shows, as a whole or one of its monitors | tela ("Escolher tela", "Tela 2") | screen; one of several: display ("Display 2") | |
| One of this computer's own monitors (where full screen opens) | monitor | monitor | tela (for this computer's monitors) |
| A connection between two computers, from its start to its end | sessão | session | conexão (for the whole session); connection (for the whole session) |
| Seeing and using the other computer | controlar ("você controla") | control | acessar remotamente; remote into |
| Only seeing it | ver ("está vendo", "Só deixar ver") | watch, see ("Allow watching only") | visualizar; view only |
| Trading the roles of the two computers | inverter ("Pedir inversão") | swap | trocar papéis; switch roles |
| Making two computers trust each other with a code | parear, pareamento | pair, pairing | emparelhar |
| Swapdesk's own settings | Ajustes | Settings | Preferências; Preferences |
| The system's settings (Windows Settings, macOS System Settings) | Configurações (Windows), Ajustes do Sistema (macOS) | Settings, System Settings | |
| The list of what happened, on every device | Atividade | Activity | |
| That list as a file to keep | registro ("Exportar registro") | log ("Export log") | |
| What happened with one device (its tab) | Histórico | History | |
| Copy and paste between the computers | área de transferência | clipboard | pasteboard |
| Entering without the person on the other side accepting | Entrar sem perguntar | Enter without asking | acesso não assistido (in the interface); unattended (in the interface) |
| Asking the other computer for its administrator's rights in a session | elevar, controle elevado | elevate, elevated control | |
| Waking a sleeping computer over the network | acordar | wake | Wake-on-LAN (in the interface) |

Counts take the singular or the plural by the language's rule (CLDR: in pt-BR 0 and 1 are singular, in English only 1), written in a template as `{n|singular|plural}`, never as "item(ns)".
