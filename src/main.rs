use std::io::Write;
use xstory::arena;
use xstory::error::Error;
use xstory::record;
use xstory::tui;

// ── Shell Hooks ──────────────────────────────────────────────────

const BASH_HOOK: &str = r#"__xstory_preexec() {
    __xstory_start_time=$(date +%s%3N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1000))' 2>/dev/null || echo 0)
}
__xstory_precmd() {
    local exit_code=$?
    if [ -n "$__xstory_start_time" ]; then
        local now=$(date +%s%3N 2>/dev/null || python3 -c 'import time; print(int(time.time()*1000))' 2>/dev/null || echo 0)
        local dur=$((now - __xstory_start_time)); [ $dur -lt 0 ] && dur=0
        local last_cmd=$(HISTTIMEFORMAT= history 1 | sed 's/^[ ]*[0-9]*[ ]*//')
        xstory record "$last_cmd" --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
        unset __xstory_start_time
    fi
}
__xstory_search() {
    local cmd; cmd=$(xstory search --query="$READLINE_LINE"); local ret=$?
    if ([ $ret -eq 0 ] || [ $ret -eq 2 ]) && [ -n "$cmd" ]; then
        READLINE_LINE="$cmd"; READLINE_POINT=${#READLINE_LINE}
    fi
}
bind -x '"\C-r": __xstory_search'
"#;

const ZSH_HOOK: &str = r#"autoload -Uz add-zsh-hook
__xstory_preexec() { __xstory_cmd="$1"; __xstory_start_ms=$((EPOCHREALTIME * 1000)); }
__xstory_precmd() {
    local exit_code=$?
    if [[ -n "$__xstory_cmd" ]]; then
        local now_ms=$((EPOCHREALTIME * 1000))
        local dur=$((now_ms - __xstory_start_ms)); (( dur < 0 )) && dur=0
        xstory record "$__xstory_cmd" --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
        unset __xstory_cmd; unset __xstory_start_ms
    fi
}
add-zsh-hook preexec __xstory_preexec; add-zsh-hook precmd __xstory_precmd
__xstory_widget() {
    local selected; selected=$(xstory search --query="$BUFFER"); local ret=$?
    if [[ ($ret -eq 0 || $ret -eq 2) && -n "$selected" ]]; then
        BUFFER="$selected"; CURSOR=${#BUFFER}
        [[ $ret -eq 0 ]] && zle accept-line || zle reset-prompt
    else zle reset-prompt; fi
}
zle -N __xstory_widget; bindkey '^R' __xstory_widget
"#;

const FISH_HOOK: &str = r#"function __xstory_postexec --on-event fish_postexec
    set -l exit_code $status; set -l dur 0
    test -n "$CMD_DURATION" && set dur $CMD_DURATION
    xstory record "$argv[1]" --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
end
function __xstory_search
    set -l cmd (commandline); set -l selected (xstory search --query="$cmd"); set -l ret $status
    if test $ret -eq 0
        commandline -r "$selected"; commandline -f execute
    else if test $ret -eq 2
        commandline -r "$selected"; commandline -f repaint
    end
end
bind \cr __xstory_search
"#;

// ── Signal Handling (Unix) ───────────────────────────────────────

#[cfg(unix)]
mod signals {
    unsafe extern "C" { fn signal(s: i32, h: usize) -> usize; fn raise(s: i32) -> i32; }
    extern "C" fn handle_signal(sig: i32) { crate::tui::reset_terminal(); unsafe { signal(sig, 0); let _ = raise(sig); } }
    pub fn install() { unsafe { signal(2, handle_signal as *const () as usize); signal(15, handle_signal as *const () as usize); signal(28, 1); } }
}

// ── Panic Hook ───────────────────────────────────────────────────

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| { tui::reset_terminal(); default(info); }));
}

// ── CLI Actions ──────────────────────────────────────────────────

enum Action {
    Record { cmd_parts: Vec<String>, exit_code: i32, duration_ms: u64, cwd: Option<String> },
    Search { initial_query: Option<String> },
    Init { shell: String },
    Help,
    Version,
}

// ── Argument Parsing ─────────────────────────────────────────────

fn parse_args(args: &[String]) -> Result<Action, Error> {
    let Some(subcmd) = args.first().map(String::as_str) else {
        return Ok(Action::Search { initial_query: None });
    };
    match subcmd {
        "-h" | "--help" | "help" => Ok(Action::Help),
        "-v" | "--version" | "version" => Ok(Action::Version),
        "init" => match args.get(1).map(|s| s.to_lowercase()).as_deref() {
            Some(s @ ("bash" | "zsh" | "fish")) => Ok(Action::Init { shell: s.to_owned() }),
            Some(_) => Err(Error::InvalidArgs("unsupported shell (supported: bash, zsh, fish)")),
            None => Err(Error::InvalidArgs("missing shell argument for init (expected bash, zsh, or fish)")),
        },
        "search" => {
            let (mut q, mut it) = (None, args[1..].iter());
            while let Some(arg) = it.next() {
                if let Some(val) = arg.strip_prefix("--query=") { q = Some(val.to_owned()); }
                else if arg == "--query" || arg == "-q" { q = it.next().cloned(); }
                else if !arg.starts_with('-') && q.is_none() { q = Some(arg.clone()); }
            }
            Ok(Action::Search { initial_query: q })
        }
        "record" => {
            let (mut exit, mut dur, mut cwd, mut pos, mut dd) = (None, None, None, Vec::new(), false);
            let mut it = args[1..].iter();
            while let Some(a) = it.next() {
                if dd { pos.push(a.clone()); }
                else if a == "--" { dd = true; }
                else if let Some(v) = a.strip_prefix("--exit=") { exit = v.parse().ok(); }
                else if let Some(v) = a.strip_prefix("--duration=") { dur = v.parse().ok(); }
                else if let Some(v) = a.strip_prefix("--cwd=") { cwd = Some(v.to_owned()); }
                else if a == "--exit" { exit = it.next().and_then(|s| s.parse().ok()); }
                else if a == "--duration" { dur = it.next().and_then(|s| s.parse().ok()); }
                else if a == "--cwd" { cwd = it.next().cloned(); }
                else { pos.push(a.clone()); }
            }
            let (cmd_parts, exit_code, duration_ms) = if exit.is_none() || dur.is_none() {
                let n = pos.len();
                if n >= 3 && let (Ok(e), Ok(d)) = (pos[1].parse::<i32>(), pos[2].parse::<u64>()) {
                    if n > 3 && cwd.is_none() { cwd = Some(pos[3].clone()); }
                    (vec![pos[0].clone()], e, d)
                } else if n >= 3 && let (Ok(e), Ok(d)) = (pos[n - 2].parse::<i32>(), pos[n - 1].parse::<u64>()) {
                    (pos[..n - 2].to_vec(), e, d)
                } else {
                    (pos, exit.unwrap_or(0), dur.unwrap_or(0))
                }
            } else {
                (pos, exit.unwrap_or(0), dur.unwrap_or(0))
            };
            Ok(Action::Record { cmd_parts, exit_code, duration_ms, cwd })
        }
        _ => Err(Error::InvalidArgs(if subcmd.starts_with('-') { "unknown option" } else { "unknown subcommand" })),
    }
}

fn print_help() {
    println!(
        "xstory 0.1.0 — High-performance shell history utility\n\n\
        USAGE:\n    \
            xstory record <command...> --exit=<i32> --duration=<u64> [--cwd=<path>]\n    \
            xstory search [--query=<str>]\n    \
            xstory init <bash|zsh|fish>\n    \
            xstory --help\n    \
            xstory --version"
    );
}

// ── Main Entrypoint ──────────────────────────────────────────────

fn main() {
    if let Err(e) = run() {
        eprintln!("xstory: {e}");
        std::process::exit(e.exit_code());
    }
}

fn run() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(&args)? {
        Action::Help => { print_help(); Ok(()) }
        Action::Version => { println!("xstory 0.1.0"); Ok(()) }
        Action::Init { shell } => {
            let hook = match shell.as_str() {
                "bash" => BASH_HOOK,
                "zsh" => ZSH_HOOK,
                "fish" => FISH_HOOK,
                _ => "",
            };
            print!("{hook}");
            Ok(())
        }
        Action::Record { cmd_parts, exit_code, duration_ms, cwd } => {
            let config = arena::Config::load();
            record::record(&cmd_parts, exit_code, duration_ms, cwd.as_deref(), &config)
        }
        Action::Search { initial_query } => {
            install_panic_hook();
            #[cfg(unix)]
            signals::install();

            let config = arena::Config::load();
            let current_dir = std::env::var("PWD").ok().or_else(|| {
                std::env::current_dir().ok().and_then(|p| p.into_os_string().into_string().ok())
            });

            let arena = arena::Arena::load(&config.history_path, current_dir.as_deref())?;
            let outcome = tui::run_tui(&arena, &config, initial_query.as_deref())?;

            let (cmd, code) = match outcome {
                tui::Outcome::Execute(c) => (Some(c), 0),
                tui::Outcome::Edit(c) => (Some(c), 2),
                tui::Outcome::Cancel => (None, 130),
            };
            if let Some(c) = cmd {
                print!("{c}");
                let _ = std::io::stdout().flush();
            }
            std::process::exit(code);
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_help_version() {
        assert!(matches!(parse_args(&["--help".into()]), Ok(Action::Help)));
        assert!(matches!(parse_args(&["-h".into()]), Ok(Action::Help)));
        assert!(matches!(parse_args(&["--version".into()]), Ok(Action::Version)));
        assert!(matches!(parse_args(&["-v".into()]), Ok(Action::Version)));
    }

    #[test]
    fn parse_empty_args_defaults_to_search() {
        match parse_args(&[]) {
            Ok(Action::Search { initial_query }) => assert!(initial_query.is_none()),
            _ => panic!("expected search"),
        }
    }

    #[test]
    fn parse_search_query() {
        match parse_args(&["search".into(), "--query=git".into()]) {
            Ok(Action::Search { initial_query }) => assert_eq!(initial_query.as_deref(), Some("git")),
            _ => panic!("expected search with query"),
        }
        match parse_args(&["search".into(), "-q".into(), "cargo".into()]) {
            Ok(Action::Search { initial_query }) => assert_eq!(initial_query.as_deref(), Some("cargo")),
            _ => panic!("expected search with query"),
        }
    }

    #[test]
    fn parse_record_flags() {
        let args = vec![
            "record".into(),
            "git".into(),
            "status".into(),
            "--exit=0".into(),
            "--duration=150".into(),
            "--cwd=/home".into(),
        ];
        match parse_args(&args) {
            Ok(Action::Record { cmd_parts, exit_code, duration_ms, cwd }) => {
                assert_eq!(cmd_parts, vec!["git", "status"]);
                assert_eq!(exit_code, 0);
                assert_eq!(duration_ms, 150);
                assert_eq!(cwd.as_deref(), Some("/home"));
            }
            _ => panic!("expected record"),
        }
    }

    #[test]
    fn parse_record_positional_double_dash() {
        let args = vec![
            "record".into(),
            "--".into(),
            "ls -la".into(),
            "0".into(),
            "100".into(),
        ];
        match parse_args(&args) {
            Ok(Action::Record { cmd_parts, exit_code, duration_ms, cwd }) => {
                assert_eq!(cmd_parts, vec!["ls -la"]);
                assert_eq!(exit_code, 0);
                assert_eq!(duration_ms, 100);
                assert!(cwd.is_none());
            }
            _ => panic!("expected record"),
        }
    }

    #[test]
    fn parse_init_shells() {
        assert!(matches!(parse_args(&["init".into(), "bash".into()]), Ok(Action::Init { shell }) if shell == "bash"));
        assert!(matches!(parse_args(&["init".into(), "zsh".into()]), Ok(Action::Init { shell }) if shell == "zsh"));
        assert!(matches!(parse_args(&["init".into(), "fish".into()]), Ok(Action::Init { shell }) if shell == "fish"));

        assert!(matches!(parse_args(&["init".into()]), Err(Error::InvalidArgs(_))));
        assert!(matches!(parse_args(&["init".into(), "powershell".into()]), Err(Error::InvalidArgs(_))));
    }

    #[test]
    fn parse_unknown_subcommand() {
        assert!(matches!(parse_args(&["foobar".into()]), Err(Error::InvalidArgs(_))));
    }

    #[test]
    fn shell_hooks_content() {
        assert!(BASH_HOOK.contains("__xstory_search"));
        assert!(ZSH_HOOK.contains("__xstory_widget"));
        assert!(FISH_HOOK.contains("__xstory_search"));
    }
}
