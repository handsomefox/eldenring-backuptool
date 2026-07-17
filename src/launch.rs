//! Steam launch-integration helpers: the launch-option string shown to the
//! user, and parsing of the wrapped game command the monitor receives.
//!
//! On Windows, Steam expands `%command%` to the game's full command line and
//! passes it to our exe. We forward it **verbatim** (so Easy Anti-Cheat's
//! `start_protected_game.exe` still runs) — see the `eac-safety` guarantee.

use std::ffi::OsString;
use std::path::Path;

pub const MONITOR_FLAG: &str = "--monitor";

/// The exact string a user pastes into Steam → Elden Ring → Properties →
/// General → Launch Options. The exe path is quoted so spaces are preserved;
/// `%command%` is passed through for Steam to expand.
pub fn launch_option(exe: &Path) -> String {
    format!("\"{}\" {MONITOR_FLAG} %command%", exe.to_string_lossy())
}

/// The game command the monitor should spawn: `program` plus its `args`,
/// exactly as Steam provided them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
}

/// Parse the arguments that follow our own program name. Returns the wrapped
/// game command when invoked as `<exe> --monitor <program> [args...]`, else
/// `None` (normal GUI launch, or `--monitor` with nothing after it).
pub fn parse_monitor_args<I, S>(args_after_program: I) -> Option<GameCommand>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut it = args_after_program.into_iter().map(Into::into);
    let first = it.next()?;
    if first.as_os_str() != std::ffi::OsStr::new(MONITOR_FLAG) {
        return None;
    }
    let program = it.next()?;
    Some(GameCommand {
        program,
        args: it.collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn launch_option_quotes_exe() {
        let s = launch_option(Path::new(r"C:\Program Files\ERSG\eldenring-backuptool.exe"));
        assert_eq!(
            s,
            r#""C:\Program Files\ERSG\eldenring-backuptool.exe" --monitor %command%"#
        );
    }

    #[test]
    fn parses_program_and_args() {
        let cmd = parse_monitor_args(os(&[
            "--monitor",
            r"C:\Games\ELDEN RING\Game\start_protected_game.exe",
            "-arg",
            "value with spaces",
        ]))
        .unwrap();
        assert_eq!(
            cmd.program,
            OsString::from(r"C:\Games\ELDEN RING\Game\start_protected_game.exe")
        );
        assert_eq!(cmd.args, os(&["-arg", "value with spaces"]));
    }

    #[test]
    fn program_only_no_args() {
        let cmd = parse_monitor_args(os(&["--monitor", "game.exe"])).unwrap();
        assert!(cmd.args.is_empty());
    }

    #[test]
    fn unicode_path_preserved() {
        let cmd =
            parse_monitor_args(os(&["--monitor", r"C:\游戏\エルデンリング\game.exe"])).unwrap();
        assert_eq!(
            cmd.program,
            OsString::from(r"C:\游戏\エルデンリング\game.exe")
        );
    }

    #[test]
    fn not_monitor_mode() {
        assert!(parse_monitor_args(os(&["--other", "x"])).is_none());
        assert!(parse_monitor_args(os(&[])).is_none());
    }

    #[test]
    fn monitor_flag_but_no_program() {
        assert!(parse_monitor_args(os(&["--monitor"])).is_none());
    }
}
