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
        local dur=$((now - __xstory_start_time))
        [ $dur -lt 0 ] && dur=0
        local last_cmd=$(HISTTIMEFORMAT= history 1 | sed 's/^[ ]*[0-9]*[ ]*//')
        xstory record "$last_cmd" --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
        unset __xstory_start_time
    fi
}
__xstory_search() {
    local cmd
    cmd=$(xstory search --query="$READLINE_LINE")
    local ret=$?
    if [ $ret -eq 0 ] && [ -n "$cmd" ]; then
        READLINE_LINE="$cmd"
        READLINE_POINT=${#READLINE_LINE}
    elif [ $ret -eq 2 ] && [ -n "$cmd" ]; then
        READLINE_LINE="$cmd"
        READLINE_POINT=${#READLINE_LINE}
    fi
}
bind -x '"\C-r": __xstory_search'
"#;

const ZSH_HOOK: &str = r#"autoload -Uz add-zsh-hook
__xstory_preexec() {
    __xstory_cmd="$1"
    __xstory_start_ms=$((EPOCHREALTIME * 1000))
}
__xstory_precmd() {
    local exit_code=$?
    if [[ -n "$__xstory_cmd" ]]; then
        local now_ms=$((EPOCHREALTIME * 1000))
        local dur=$((now_ms - __xstory_start_ms))
        (( dur < 0 )) && dur=0
        xstory record "$__xstory_cmd" --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
        unset __xstory_cmd
        unset __xstory_start_ms
    fi
}
add-zsh-hook preexec __xstory_preexec
add-zsh-hook precmd __xstory_precmd
__xstory_widget() {
    local selected
    selected=$(xstory search --query="$BUFFER")
    local ret=$?
    if [[ $ret -eq 0 && -n "$selected" ]]; then
        BUFFER="$selected"
        CURSOR=${#BUFFER}
        zle accept-line
    elif [[ $ret -eq 2 && -n "$selected" ]]; then
        BUFFER="$selected"
        CURSOR=${#BUFFER}
        zle reset-prompt
    else
        zle reset-prompt
    fi
}
zle -N __xstory_widget
bindkey '^R' __xstory_widget
"#;

const FISH_HOOK: &str = r#"function __xstory_postexec --on-event fish_postexec
    set -l exit_code $status
    set -l dur 0
    if test -n "$CMD_DURATION"
        set dur $CMD_DURATION
    end
    xstory record "$argv[1]" --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
end
function __xstory_search
    set -l cmd (commandline)
    set -l selected (xstory search --query="$cmd")
    set -l ret $status
    if test $ret -eq 0
        commandline -r "$selected"
        commandline -f execute
    else if test $ret -eq 2
        commandline -r "$selected"
        commandline -f repaint
    end
end
bind \cr __xstory_search
"#;

// ── Signal Handling (Unix) ───────────────────────────────────────

#[cfg(unix)]
mod signals {
    use std::io::Write;

    unsafe extern "C" {
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> extern "C" fn(i32);
        fn raise(sig: i32) -> i32;
    }

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    const SIGWINCH: i32 = 28;
    const SIG_DFL: usize = 0;
    const SIG_IGN: usize = 1;

    extern "C" fn handle_signal(sig: i32) {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut stderr = std::io::stderr();
        let _ = crossterm::execute!(
            stderr,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = stderr.flush();

        // SAFETY: reset disposition to default (SIG_DFL) and raise signal so parent shell gets real signal
        unsafe {
            signal(sig, std::mem::transmute::<usize, extern "C" fn(i32)>(SIG_DFL));
            let _ = raise(sig);
        }
    }

    pub fn install() {
        // SAFETY: standard C POSIX signal installation for graceful terminal restoration on exit
        unsafe {
            signal(SIGINT, handle_signal);
            signal(SIGTERM, handle_signal);
            signal(SIGWINCH, std::mem::transmute::<usize, extern "C" fn(i32)>(SIG_IGN));
        }
    }
}

// ── Panic Hook ───────────────────────────────────────────────────

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let mut stderr = std::io::stderr();
        let _ = crossterm::execute!(
            stderr,
            crossterm::event::DisableMouseCapture,
            crossterm::cursor::Show,
            crossterm::terminal::LeaveAlternateScreen
        );
        let _ = stderr.flush();
        default_hook(info);
    }));
}

// ── CLI Actions ──────────────────────────────────────────────────

enum Action {
    Record {
        cmd_parts: Vec<String>,
        exit_code: i32,
        duration_ms: u64,
        cwd: Option<String>,
    },
    Search {
        initial_query: Option<String>,
    },
    Init {
        shell: String,
    },
    Help,
    Version,
}

// ── Argument Parsing ─────────────────────────────────────────────

#[allow(clippy::too_many_lines)]
fn parse_args(args: &[String]) -> Result<Action, Error> {
    if args.is_empty() {
        return Ok(Action::Search { initial_query: None });
    }

    let subcmd = args[0].as_str();
    match subcmd {
        "-h" | "--help" | "help" => Ok(Action::Help),
        "-v" | "--version" | "version" => Ok(Action::Version),
        "search" => {
            let mut initial_query = None;
            let mut i = 1;
            while i < args.len() {
                let arg = &args[i];
                if let Some(q) = arg.strip_prefix("--query=") {
                    initial_query = Some(q.to_owned());
                } else if (arg == "--query" || arg == "-q") && i + 1 < args.len() {
                    initial_query = Some(args[i + 1].clone());
                    i += 1;
                } else if !arg.starts_with('-') && initial_query.is_none() {
                    initial_query = Some(arg.clone());
                }
                i += 1;
            }
            Ok(Action::Search { initial_query })
        }
        "record" => {
            let mut cmd_parts = Vec::new();
            let mut exit_code: Option<i32> = None;
            let mut duration_ms: Option<u64> = None;
            let mut cwd: Option<String> = None;

            let mut i = 1;
            let mut positional = Vec::new();
            let mut after_double_dash = false;

            while i < args.len() {
                let arg = &args[i];
                if after_double_dash {
                    positional.push(arg.clone());
                } else if arg == "--" {
                    after_double_dash = true;
                } else if let Some(val) = arg.strip_prefix("--exit=") {
                    exit_code = val.parse().ok();
                } else if let Some(val) = arg.strip_prefix("--duration=") {
                    duration_ms = val.parse().ok();
                } else if let Some(val) = arg.strip_prefix("--cwd=") {
                    cwd = Some(val.to_owned());
                } else if arg == "--exit" && i + 1 < args.len() {
                    exit_code = args[i + 1].parse().ok();
                    i += 1;
                } else if arg == "--duration" && i + 1 < args.len() {
                    duration_ms = args[i + 1].parse().ok();
                    i += 1;
                } else if arg == "--cwd" && i + 1 < args.len() {
                    cwd = Some(args[i + 1].clone());
                    i += 1;
                } else {
                    positional.push(arg.clone());
                }
                i += 1;
            }

            if exit_code.is_none() || duration_ms.is_none() {
                let len = positional.len();
                if len >= 3
                    && let (Ok(e), Ok(d)) = (positional[1].parse::<i32>(), positional[2].parse::<u64>())
                {
                    cmd_parts.push(positional[0].clone());
                    exit_code = Some(e);
                    duration_ms = Some(d);
                    if len > 3 && cwd.is_none() {
                        cwd = Some(positional[3].clone());
                    }
                } else if len >= 3
                    && let (Ok(e), Ok(d)) = (positional[len - 2].parse::<i32>(), positional[len - 1].parse::<u64>())
                {
                    cmd_parts.extend(positional[..len - 2].iter().cloned());
                    exit_code = Some(e);
                    duration_ms = Some(d);
                } else {
                    cmd_parts = positional;
                }
            } else {
                cmd_parts = positional;
            }

            let exit = exit_code.unwrap_or(0);
            let dur = duration_ms.unwrap_or(0);
            Ok(Action::Record {
                cmd_parts,
                exit_code: exit,
                duration_ms: dur,
                cwd,
            })
        }
        "init" => {
            if args.len() < 2 {
                return Err(Error::InvalidArgs("missing shell argument for init (expected bash, zsh, or fish)"));
            }
            let shell = args[1].to_lowercase();
            if shell != "bash" && shell != "zsh" && shell != "fish" {
                return Err(Error::InvalidArgs("unsupported shell (supported: bash, zsh, fish)"));
            }
            Ok(Action::Init { shell })
        }
        _ => {
            if subcmd.starts_with('-') {
                Err(Error::InvalidArgs("unknown option"))
            } else {
                Err(Error::InvalidArgs("unknown subcommand"))
            }
        }
    }
}

fn print_help() {
    println!(
        "xstory 0.1.0 — High-performance shell history utility\n\
        \n\
        USAGE:\n    \
            xstory record <command...> --exit=<i32> --duration=<u64> [--cwd=<path>]\n    \
            xstory search [--query=<str>]\n    \
            xstory init <bash|zsh|fish>\n    \
            xstory --help\n    \
            xstory --version"
    );
}

fn print_version() {
    println!("xstory 0.1.0");
}

fn print_shell_hook(shell: &str) {
    match shell {
        "bash" => print!("{BASH_HOOK}"),
        "zsh" => print!("{ZSH_HOOK}"),
        "fish" => print!("{FISH_HOOK}"),
        _ => {}
    }
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
    let action = parse_args(&args)?;

    match action {
        Action::Help => {
            print_help();
            Ok(())
        }
        Action::Version => {
            print_version();
            Ok(())
        }
        Action::Init { shell } => {
            print_shell_hook(&shell);
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

            match outcome {
                tui::Outcome::Execute(cmd) => {
                    println!("{cmd}");
                    let _ = std::io::stdout().flush();
                    std::process::exit(0);
                }
                tui::Outcome::Edit(cmd) => {
                    println!("{cmd}");
                    let _ = std::io::stdout().flush();
                    std::process::exit(2);
                }
                tui::Outcome::Cancel => {
                    std::process::exit(130);
                }
            }
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
