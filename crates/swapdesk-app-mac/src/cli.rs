//! The command line of `Swapdesk.app`'s executable. Unknown arguments are ignored (macOS adds its own, such as `-psn_0_1`
//! or `-NSDocumentRevisionsDebugMode`).
//!
//! - `--mock <scenario>`: the scripted `MockBackend`, no service (looking at the screens).
//! - `--scratch`: development. The secrets go to a throwaway keychain file and folder in the temp directory, the
//!   ports are chosen by the system and nothing of the owner's real Swapdesk identity is touched; both are removed at exit.
//! - `--selftest <file>`: development. Implies `--scratch`. Once the main window is up the app writes what it sees (the
//!   windows, the service) to `<file>` and quits by itself. With `--mock <scenario>` it checks the interface alone: a
//!   click, typing in a text field and a rename on the mock backend.
//! - `--drive-host <dir>`: development, only in a build with the `dev-harness` feature. Implies `--scratch` but keeps
//!   the real ports (47820/47821): a scripted user pairs with the viewer through code files in `<dir>`, accepts its
//!   session and quits when it ends (`host.log` in `<dir>` holds the markers and `HOST_RESULT`).
//! - `--ui-script <dir>`: development, only in a build with the `dev-harness` feature. Implies `--scratch`: the real
//!   app with a throwaway identity runs the commands a script writes into `<dir>` (clicks, keys, the mouse and keys
//!   on the viewer's video), so another machine can drive it as a person would (`ui_script.rs`).
//! - `--help`, `-h`: the usage text on stdout.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Options {
    pub mock: Option<String>,
    pub scratch: bool,
    pub selftest: Option<PathBuf>,
    pub drive_host: Option<PathBuf>,
    pub ui_script: Option<PathBuf>,
    /// Development, with `--mock`: capture the main window to this file once drawn, then quit.
    pub shot: Option<PathBuf>,
    pub help: bool,
}

impl Options {
    /// Scratch secrets: asked for, or implied by the self-test, the host drive or the UI script.
    pub fn use_scratch(&self) -> bool {
        self.scratch || self.selftest.is_some() || self.drive_host.is_some() || self.ui_script.is_some()
    }

    /// Only a normal run registers the app as a login item (D32): never the mock, a scratch run, the self-test or the
    /// host drive, which would add a login item for a development copy.
    pub fn registers_login_item(&self) -> bool {
        self.mock.is_none() && !self.use_scratch()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    MissingValue(&'static str),
}

impl core::fmt::Display for CliError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CliError::MissingValue(flag) => write!(f, "{flag} needs a value."),
        }
    }
}

pub const USAGE: &str = "Swapdesk [--scratch] [--selftest <file>] [--drive-host <dir>] [--ui-script <dir>] | Swapdesk --mock <scenario> [--shot <png>] [--selftest <file>]";

/// Parses the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Options, CliError> {
    let mut o = Options::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mock" => {
                o.mock = Some(args.get(i + 1).ok_or(CliError::MissingValue("--mock"))?.clone());
                i += 1;
            }
            "--selftest" => {
                o.selftest = Some(PathBuf::from(args.get(i + 1).ok_or(CliError::MissingValue("--selftest"))?));
                i += 1;
            }
            "--drive-host" => {
                o.drive_host = Some(PathBuf::from(args.get(i + 1).ok_or(CliError::MissingValue("--drive-host"))?));
                i += 1;
            }
            "--ui-script" => {
                o.ui_script = Some(PathBuf::from(args.get(i + 1).ok_or(CliError::MissingValue("--ui-script"))?));
                i += 1;
            }
            "--shot" => {
                o.shot = Some(PathBuf::from(args.get(i + 1).ok_or(CliError::MissingValue("--shot"))?));
                i += 1;
            }
            "--scratch" => o.scratch = true,
            "--help" | "-h" => o.help = true,
            _ => {}
        }
        i += 1;
    }
    Ok(o)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_shot_goes_with_a_mock_scenario() {
        let o = parse(&args(&["--mock", "parear-este-computador", "--shot", "/tmp/p.png"])).unwrap();
        assert_eq!((o.mock.as_deref(), o.shot), (Some("parear-este-computador"), Some(PathBuf::from("/tmp/p.png"))));
        assert_eq!(parse(&args(&["--shot"])), Err(CliError::MissingValue("--shot")));
    }

    #[test]
    fn no_arguments_means_production() {
        let o = parse(&[]).unwrap();
        assert_eq!(o, Options::default());
        assert!(!o.use_scratch());
    }

    #[test]
    fn the_selftest_implies_scratch_and_macos_arguments_are_ignored() {
        let o = parse(&args(&["-psn_0_12345", "--selftest", "/tmp/x.txt", "-NSDocumentRevisionsDebugMode", "YES"]))
            .unwrap();
        assert_eq!(o.selftest, Some(PathBuf::from("/tmp/x.txt")));
        assert!(o.use_scratch() && !o.scratch);
    }

    #[test]
    fn mock_scratch_and_help_parse_in_any_order() {
        let o = parse(&args(&["--scratch", "--help", "--mock", "visualizador"])).unwrap();
        assert_eq!(
            o,
            Options {
                mock: Some("visualizador".into()),
                scratch: true,
                selftest: None,
                drive_host: None,
                ui_script: None,
                shot: None,
                help: true
            }
        );
    }

    #[test]
    fn the_host_drive_implies_scratch() {
        let o = parse(&args(&["--drive-host", "/tmp/drive"])).unwrap();
        assert_eq!(o.drive_host, Some(PathBuf::from("/tmp/drive")));
        assert!(o.use_scratch() && !o.scratch);
        assert_eq!(parse(&args(&["--drive-host"])), Err(CliError::MissingValue("--drive-host")));
    }

    #[test]
    fn only_a_normal_run_registers_the_login_item() {
        assert!(parse(&[]).unwrap().registers_login_item());
        assert!(parse(&args(&["-psn_0_1"])).unwrap().registers_login_item());
        for flags in [&["--scratch"][..], &["--selftest", "/tmp/x"], &["--drive-host", "/tmp/d"], &["--mock", "pronto"]]
        {
            assert!(!parse(&args(flags)).unwrap().registers_login_item(), "{flags:?}");
        }
    }

    #[test]
    fn a_missing_value_is_an_error() {
        assert_eq!(parse(&args(&["--mock"])), Err(CliError::MissingValue("--mock")));
        assert_eq!(parse(&args(&["--selftest"])), Err(CliError::MissingValue("--selftest")));
    }
}
