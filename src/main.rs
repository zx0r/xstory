use std::io::Write;
use std::path::PathBuf;
use xstory::arena;
use xstory::error::Error;
use xstory::record;
use xstory::tui;

// ── Shell Hooks ──────────────────────────────────────────────────

const BASH_HOOK: &str = r#"[[ $- == *i* ]] || return 0
[[ -n "${XSTORY_DISABLE-}" ]] && return 0
command -v xstory >/dev/null 2>&1 || return 0

__xstory_init() {
    local -i __xstory_last_histcmd=0
    local __xstory_cmd=""

    __xstory_preexec() {
        __xstory_cmd="$BASH_COMMAND"
        if (( BASH_VERSINFO[0] >= 5 )); then
            __xstory_start_time=${EPOCHREALTIME-}
        else
            __xstory_start_time=$SECONDS
        fi
    }

    __xstory_precmd() {
        local exit_code=$1
        if (( HISTCMD <= __xstory_last_histcmd )); then
            __xstory_cmd=""
            unset __xstory_start_time
            return 0
        fi
        __xstory_last_histcmd=$HISTCMD

        if [[ -n "${__xstory_cmd-}" && -n "${__xstory_start_time-}" ]]; then
            local dur=0
            if (( BASH_VERSINFO[0] >= 5 )) && [[ -n "${EPOCHREALTIME-}" ]]; then
                local start_us=${__xstory_start_time//[!0-9]}
                local now_us=${EPOCHREALTIME//[!0-9]}
                dur=$(( (now_us - start_us) / 1000 ))
                (( dur < 0 )) && dur=0
            else
                local dur_s=$(( SECONDS - __xstory_start_time ))
                (( dur_s < 0 )) && dur_s=0
                dur=$(( dur_s * 1000 ))
            fi

            if [[ "$__xstory_cmd" != [[:space:]]* && "$__xstory_cmd" != "__xstory"* ]]; then
                printf '%s' "$__xstory_cmd" | xstory record --stdin --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
            fi
            __xstory_cmd=""
            unset __xstory_start_time
        fi
    }

    if [[ -n "${BLE_VERSION-}" ]]; then
        blehook PREEXEC!='__xstory_preexec'
        blehook PRECMD!='__xstory_precmd "$?"'
    elif [[ -n "${bash_preexec_imported-}" || -n "${__bp_imported-}" ]]; then
        preexec_functions+=(__xstory_preexec)
        precmd_functions+=(__xstory_precmd_wrapper)
        __xstory_precmd_wrapper() { __xstory_precmd "$?"; }
    else
        trap '__xstory_preexec' DEBUG
        if [[ -z "${PROMPT_COMMAND-}" ]]; then
            PROMPT_COMMAND='__xstory_precmd "$?"'
        elif [[ "$PROMPT_COMMAND" != *"__xstory_precmd"* ]]; then
            PROMPT_COMMAND='__xstory_precmd "$?"; '"$PROMPT_COMMAND"
        fi
    fi

    local KEY1='\C-x1'
    local KEY2='\C-x2'

    __xstory_search_dispatch() {
        local selected ret
        selected=$(xstory -- "$READLINE_LINE")
        ret=$?
        if [[ $ret -eq 0 && -n "$selected" ]]; then
            READLINE_LINE="$selected"
            READLINE_POINT=${#READLINE_LINE}
            bind "\"$KEY2\": accept-line"
        elif [[ $ret -eq 2 && -n "$selected" ]]; then
            READLINE_LINE="$selected"
            READLINE_POINT=${#READLINE_LINE}
            bind "\"$KEY2\": \"\""
        else
            bind "\"$KEY2\": \"\""
        fi
    }

    bind -x "\"$KEY1\": __xstory_search_dispatch"
    bind "\"$KEY2\": \"\""

    bind "\"\\C-r\": \"$KEY1$KEY2\""
    bind "\"\\e[A\": \"$KEY1$KEY2\""
}
__xstory_init
unset -f __xstory_init
"#;

const ZSH_HOOK: &str = r#"[[ -o interactive ]] || return 0
[[ -n "${XSTORY_DISABLE-}" ]] && return 0
command -v xstory >/dev/null 2>&1 || return 0

zmodload zsh/datetime 2>/dev/null
autoload -Uz add-zsh-hook

typeset -g __xstory_cmd=""
typeset -F __xstory_start_time=0

__xstory_preexec() {
    __xstory_cmd="$1"
    __xstory_start_time=$EPOCHREALTIME
}

__xstory_precmd() {
    local exit_code=$?
    if [[ -n "$__xstory_cmd" ]]; then
        if [[ "$__xstory_cmd" != [[:space:]]* ]]; then
            local -F now=$EPOCHREALTIME
            local -i dur=$(( (now - __xstory_start_time) * 1000 ))
            (( dur < 0 )) && dur=0
            print -rn -- "$__xstory_cmd" | xstory record --stdin --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
        fi
        __xstory_cmd=""
        __xstory_start_time=0
    fi
}

add-zsh-hook preexec __xstory_preexec
add-zsh-hook precmd __xstory_precmd

__xstory_widget() {
    emulate -L zsh
    zle -I
    local selected ret
    selected=$(xstory -- "$BUFFER")
    ret=$?
    if [[ ($ret -eq 0 || $ret -eq 2) && -n "$selected" ]]; then
        BUFFER="$selected"
        CURSOR=${#BUFFER}
        if [[ $ret -eq 0 ]]; then
            zle accept-line
        else
            zle reset-prompt
        fi
    else
        zle reset-prompt
    fi
}

zle -N __xstory_widget
bindkey '^R' __xstory_widget
bindkey '^[[A' __xstory_widget
bindkey -M viins '^R' __xstory_widget
bindkey -M viins '^[[A' __xstory_widget
bindkey -M vicmd '^R' __xstory_widget
bindkey -M vicmd '^[[A' __xstory_widget

if [[ -n "${ZVM_VERSION-}" ]]; then
    zvm_after_init_commands+=(
        "bindkey -M viins '^R' __xstory_widget"
        "bindkey -M vicmd '^R' __xstory_widget"
    )
fi
"#;

const FISH_HOOK: &str = r#"status is-interactive; or return 0
set -q XSTORY_DISABLE; and return 0
command -q xstory; or return 0

function __xstory_postexec --on-event fish_postexec
    set -l exit_code $status
    if set -q fish_private_mode
        return
    end
    if string match -qr '^\s' -- "$argv[1]"
        return
    end
    set -l dur 0
    test -n "$CMD_DURATION"; and set dur $CMD_DURATION
    printf '%s' "$argv[1]" | xstory record --stdin --exit="$exit_code" --duration="$dur" --cwd="$PWD" >/dev/null 2>&1
end

function __xstory_search
    set -l selected (xstory -- (commandline -b) | string collect)
    set -l ret $status
    if test $ret -eq 0 -a -n "$selected"
        commandline -r -- "$selected"
        commandline -f repaint
        commandline -f execute
    else if test $ret -eq 2 -a -n "$selected"
        commandline -r -- "$selected"
        commandline -f repaint
    else
        commandline -f repaint
    end
end

bind up __xstory_search
bind \cr __xstory_search
if bind -M insert >/dev/null 2>&1
    bind -M insert up __xstory_search
    bind -M insert \cr __xstory_search
end
"#;

// ── Signal Handling (Unix) ───────────────────────────────────────

#[cfg(unix)]
#[allow(unsafe_code)]
mod signals {
    unsafe extern "C" {
        fn signal(s: i32, h: usize) -> usize;
        fn raise(s: i32) -> i32;
        fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    }
    extern "C" fn handle_signal(sig: i32) {
        // Direct, async-signal-safe raw ANSI write to stderr (fd 2):
        // 1. Disable mouse capture (\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1015l)
        // 2. Show cursor (\x1b[?25h)
        // 3. Leave alternate screen (\x1b[?1049l)
        const RESET_SEQ: &[u8] =
            b"\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1015l\x1b[?25h\x1b[?1049l";
        // SAFETY: `write` is async-signal-safe per POSIX.1-2008; avoids stderr mutex deadlock.
        unsafe {
            let _ = write(2, RESET_SEQ.as_ptr(), RESET_SEQ.len());
        }
        // SAFETY: Restoring default signal disposition before re-raising.
        // `signal(sig, SIG_DFL)` is async-signal-safe per POSIX.
        unsafe {
            signal(sig, 0);
        }
        // SAFETY: `raise` is async-signal-safe per POSIX.
        unsafe {
            let _ = raise(sig);
        }
    }
    #[allow(clippy::redundant_pub_crate)]
    pub(crate) fn install() {
        // SAFETY: Installing C-ABI handler for SIGINT (2).
        unsafe {
            signal(2, handle_signal as *const () as usize);
        }
        // SAFETY: Installing C-ABI handler for SIGTERM (15).
        unsafe {
            signal(15, handle_signal as *const () as usize);
        }
        // SAFETY: Setting SIGWINCH (28) to SIG_IGN (1) — we poll size ourselves.
        unsafe {
            signal(28, 1);
        }
    }
}

// ── Panic Hook ───────────────────────────────────────────────────

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tui::reset_terminal();
        default(info);
    }));
}

// ── CLI Actions ──────────────────────────────────────────────────

enum Action {
    Record {
        cmd_parts: Vec<String>,
        exit_code: i32,
        duration_ms: u64,
        cwd: Option<String>,
        from_stdin: bool,
    },
    Search {
        initial_query: Option<String>,
    },
    Init {
        shell: String,
    },
    HistoryImport {
        path: Option<String>,
    },
    HistoryExport {
        path: Option<String>,
    },
    Help,
    Version,
}

struct Cli {
    history_path: Option<PathBuf>,
    scope: Option<arena::Scope>,
    preview: Option<arena::PreviewMode>,
    action: Action,
}

fn parse_preview_mode(val: &str) -> Result<arena::PreviewMode, Error> {
    match val.to_lowercase().as_str() {
        "dynamic" => Ok(arena::PreviewMode::Dynamic),
        "fixed" => Ok(arena::PreviewMode::Fixed),
        _ => Err(Error::InvalidArgs(
            "invalid preview mode (expected dynamic or fixed)",
        )),
    }
}

// ── Argument Parsing ─────────────────────────────────────────────

fn parse_record_args(args: &[String]) -> Action {
    let (mut exit, mut dur, mut cwd, mut pos, mut dd, mut from_stdin) =
        (None, None, None, Vec::new(), false, false);
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if dd {
            pos.push(a.clone());
        } else if a == "--" {
            dd = true;
        } else if a == "--stdin" {
            from_stdin = true;
        } else if let Some(v) = a.strip_prefix("--exit=") {
            exit = v.parse().ok();
        } else if let Some(v) = a.strip_prefix("--duration=") {
            dur = v.parse().ok();
        } else if let Some(v) = a.strip_prefix("--cwd=") {
            cwd = Some(v.to_owned());
        } else if a == "--exit" {
            exit = iter.next().and_then(|s| s.parse().ok());
        } else if a == "--duration" {
            dur = iter.next().and_then(|s| s.parse().ok());
        } else if a == "--cwd" {
            cwd = iter.next().cloned();
        } else {
            pos.push(a.clone());
        }
    }
    let (cmd_parts, exit_code, duration_ms) = if exit.is_none() || dur.is_none() {
        let n = pos.len();
        if n >= 3
            && let (Ok(e), Ok(d)) = (pos[1].parse::<i32>(), pos[2].parse::<u64>())
        {
            if n > 3 && cwd.is_none() {
                cwd = Some(pos[3].clone());
            }
            (vec![pos[0].clone()], e, d)
        } else if n >= 3
            && let (Ok(e), Ok(d)) = (pos[n - 2].parse::<i32>(), pos[n - 1].parse::<u64>())
        {
            (pos[..n - 2].to_vec(), e, d)
        } else {
            (pos, exit.unwrap_or(0), dur.unwrap_or(0))
        }
    } else {
        (pos, exit.unwrap_or(0), dur.unwrap_or(0))
    };
    Action::Record {
        cmd_parts,
        exit_code,
        duration_ms,
        cwd,
        from_stdin,
    }
}

#[allow(clippy::too_many_lines)]
fn parse_args(args: &[String]) -> Result<Cli, Error> {
    let mut history_path = None;
    let mut scope = None;
    let mut preview = None;
    let mut rest = Vec::new();
    let mut it = args.iter();

    while let Some(arg) = it.next() {
        if arg == "-h" || arg == "--help" {
            return Ok(Cli {
                history_path,
                scope,
                preview,
                action: Action::Help,
            });
        } else if arg == "-v" || arg == "-V" || arg == "--version" {
            return Ok(Cli {
                history_path,
                scope,
                preview,
                action: Action::Version,
            });
        } else if let Some(val) = arg.strip_prefix("--history=") {
            history_path = Some(PathBuf::from(val));
        } else if arg == "-H" || arg == "--history" {
            let val = it
                .next()
                .ok_or(Error::InvalidArgs("missing argument for --history"))?;
            history_path = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--scope=") {
            scope = match val.to_lowercase().as_str() {
                "global" => Some(arena::Scope::Global),
                "local" => Some(arena::Scope::Local),
                _ => {
                    return Err(Error::InvalidArgs(
                        "invalid scope (expected global or local)",
                    ));
                }
            };
        } else if arg == "-s" || arg == "--scope" {
            let val = it
                .next()
                .ok_or(Error::InvalidArgs("missing argument for --scope"))?;
            scope = match val.to_lowercase().as_str() {
                "global" => Some(arena::Scope::Global),
                "local" => Some(arena::Scope::Local),
                _ => {
                    return Err(Error::InvalidArgs(
                        "invalid scope (expected global or local)",
                    ));
                }
            };
        } else if let Some(val) = arg
            .strip_prefix("--preview=")
            .or_else(|| arg.strip_prefix("--preview-mode="))
        {
            preview = Some(parse_preview_mode(val)?);
        } else if arg == "-p" || arg == "--preview" || arg == "--preview-mode" {
            let val = it
                .next()
                .ok_or(Error::InvalidArgs("missing argument for --preview"))?;
            preview = Some(parse_preview_mode(val)?);
        } else {
            rest.push(arg.clone());
            for rem in it {
                rest.push(rem.clone());
            }
            break;
        }
    }

    let Some(subcmd) = rest.first().map(String::as_str) else {
        return Ok(Cli {
            history_path,
            scope,
            preview,
            action: Action::Search {
                initial_query: None,
            },
        });
    };

    let action = match subcmd {
        "history" => {
            let sub = rest.get(1).map(String::as_str);
            let extra = rest.get(2).map(String::as_str);
            if rest.len() > 3 || extra.is_some_and(|p| p.starts_with('-') && p != "-") {
                return Err(Error::InvalidArgs(
                    "usage: xstory history <import|export> [path]",
                ));
            }
            match sub {
                Some("import") => Action::HistoryImport {
                    path: extra.map(str::to_owned),
                },
                Some("export") => Action::HistoryExport {
                    path: extra.map(str::to_owned),
                },
                Some("-h" | "--help") => Action::Help,
                _ => {
                    return Err(Error::InvalidArgs(
                        "usage: xstory history <import|export> [path]",
                    ));
                }
            }
        }
        "init" => match rest.get(1).map(|s| s.to_lowercase()).as_deref() {
            Some(s @ ("bash" | "zsh" | "fish")) => Action::Init {
                shell: s.to_owned(),
            },
            Some(_) => {
                return Err(Error::InvalidArgs(
                    "unsupported shell (supported: bash, zsh, fish)",
                ));
            }
            None => {
                return Err(Error::InvalidArgs(
                    "missing shell argument for init (expected bash, zsh, or fish)",
                ));
            }
        },
        "--" => {
            let q = rest.get(1).cloned();
            Action::Search { initial_query: q }
        }
        "search" => {
            let (mut q, mut iter) = (None, rest[1..].iter());
            while let Some(a) = iter.next() {
                if let Some(val) = a.strip_prefix("--query=") {
                    q = Some(val.to_owned());
                } else if a == "--query" || a == "-q" {
                    q = iter.next().cloned();
                } else if let Some(val) = a
                    .strip_prefix("--preview=")
                    .or_else(|| a.strip_prefix("--preview-mode="))
                {
                    preview = Some(parse_preview_mode(val)?);
                } else if a == "-p" || a == "--preview" || a == "--preview-mode" {
                    let val = iter
                        .next()
                        .ok_or(Error::InvalidArgs("missing argument for --preview"))?;
                    preview = Some(parse_preview_mode(val)?);
                } else if !a.starts_with('-') && q.is_none() {
                    q = Some(a.clone());
                }
            }
            Action::Search { initial_query: q }
        }
        "record" => parse_record_args(&rest[1..]),
        _ => {
            return Err(Error::InvalidArgs(if subcmd.starts_with('-') {
                "unknown option"
            } else {
                "unknown subcommand"
            }));
        }
    };

    Ok(Cli {
        history_path,
        scope,
        preview,
        action,
    })
}

fn print_help() {
    print!(
        "Minimalist Unix shell history engine\n\n\
Usage: xstory [OPTIONS] [QUERY] [COMMAND]\n\n\
Commands:\n  \
  history  Import or export history logs\n  \
  record   Record command execution into history log\n  \
  init     Initialize shell integration hook\n\n\
Arguments:\n  \
  [QUERY]  Search query\n\n\
Options:\n  \
  -H, --history <PATH>   Path to history file\n  \
  -s, --scope <SCOPE>    Search scope (global/local)\n  \
  -p, --preview <MODE>   Preview mode (dynamic/fixed)\n  \
  -h, --help             Print help\n  \
  -V, --version          Print version\n"
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
    let cli = parse_args(&args)?;
    match cli.action {
        Action::Help => {
            print_help();
            Ok(())
        }
        Action::Version => {
            println!("xstory 0.1.0");
            Ok(())
        }
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
        action => execute(action, cli.history_path, cli.scope, cli.preview),
    }
}

fn execute(
    action: Action,
    history_path_override: Option<PathBuf>,
    scope: Option<arena::Scope>,
    preview_override: Option<arena::PreviewMode>,
) -> Result<(), Error> {
    let mut config = arena::Config::load();
    if let Some(path) = history_path_override {
        config.history_path = path;
    }
    if let Some(prev) = preview_override {
        config.preview_mode = prev;
    }

    match action {
        Action::Record {
            cmd_parts,
            exit_code,
            duration_ms,
            cwd,
            from_stdin,
        } => {
            let actual_parts = if from_stdin {
                use std::io::Read;
                let mut stdin_buf = String::new();
                std::io::stdin().read_to_string(&mut stdin_buf)?;
                let cmd = stdin_buf
                    .strip_suffix("\r\n")
                    .or_else(|| stdin_buf.strip_suffix('\n'))
                    .unwrap_or(&stdin_buf);
                vec![cmd.to_owned()]
            } else {
                cmd_parts
            };
            record::record(
                &actual_parts,
                exit_code,
                duration_ms,
                cwd.as_deref(),
                &config,
            )
        }
        Action::HistoryImport { path } => {
            let count = record::import_history(path.as_deref(), &config)?;
            eprintln!("xstory: imported {count} commands");
            Ok(())
        }
        Action::HistoryExport { path } => {
            let export_scope = scope.unwrap_or(arena::Scope::Global);
            record::export_history(path.as_deref(), export_scope, &config)?;
            Ok(())
        }
        Action::Search { initial_query } => {
            install_panic_hook();
            #[cfg(unix)]
            signals::install();

            let current_dir = std::env::var("PWD").ok().or_else(|| {
                std::env::current_dir()
                    .ok()
                    .and_then(|p| p.into_os_string().into_string().ok())
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
        Action::Help | Action::Version | Action::Init { .. } => unreachable!(),
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn parse_help_version() {
        assert!(
            matches!(parse_args(&["--help".into()]), Ok(cli) if matches!(cli.action, Action::Help))
        );
        assert!(
            matches!(parse_args(&["-h".into()]), Ok(cli) if matches!(cli.action, Action::Help))
        );
        assert!(
            matches!(parse_args(&["--version".into()]), Ok(cli) if matches!(cli.action, Action::Version))
        );
        assert!(
            matches!(parse_args(&["-v".into()]), Ok(cli) if matches!(cli.action, Action::Version))
        );
        assert!(
            matches!(parse_args(&["-V".into()]), Ok(cli) if matches!(cli.action, Action::Version))
        );
    }

    #[test]
    fn parse_empty_args_defaults_to_search() {
        match parse_args(&[]) {
            Ok(cli) => match cli.action {
                Action::Search { initial_query } => assert!(initial_query.is_none()),
                _ => panic!("expected search"),
            },
            _ => panic!("expected parse success"),
        }
    }

    #[test]
    fn parse_search_query() {
        match parse_args(&["search".into(), "--query=git".into()]) {
            Ok(cli) => match cli.action {
                Action::Search { initial_query } => {
                    assert_eq!(initial_query.as_deref(), Some("git"));
                }
                _ => panic!("expected search with query"),
            },
            _ => panic!("expected parse success"),
        }
        match parse_args(&["search".into(), "-q".into(), "cargo".into()]) {
            Ok(cli) => match cli.action {
                Action::Search { initial_query } => {
                    assert_eq!(initial_query.as_deref(), Some("cargo"));
                }
                _ => panic!("expected search with query"),
            },
            _ => panic!("expected parse success"),
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
            Ok(cli) => match cli.action {
                Action::Record {
                    cmd_parts,
                    exit_code,
                    duration_ms,
                    cwd,
                    from_stdin,
                } => {
                    assert_eq!(cmd_parts, vec!["git", "status"]);
                    assert_eq!(exit_code, 0);
                    assert_eq!(duration_ms, 150);
                    assert_eq!(cwd.as_deref(), Some("/home"));
                    assert!(!from_stdin);
                }
                _ => panic!("expected record"),
            },
            _ => panic!("expected parse success"),
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
            Ok(cli) => match cli.action {
                Action::Record {
                    cmd_parts,
                    exit_code,
                    duration_ms,
                    cwd,
                    from_stdin,
                } => {
                    assert_eq!(cmd_parts, vec!["ls -la"]);
                    assert_eq!(exit_code, 0);
                    assert_eq!(duration_ms, 100);
                    assert!(cwd.is_none());
                    assert!(!from_stdin);
                }
                _ => panic!("expected record"),
            },
            _ => panic!("expected parse success"),
        }
    }

    #[test]
    fn parse_record_stdin_flag() {
        let args = vec![
            "record".into(),
            "--stdin".into(),
            "--exit=0".into(),
            "--duration=100".into(),
            "--cwd=/tmp".into(),
        ];
        match parse_args(&args) {
            Ok(cli) => match cli.action {
                Action::Record {
                    cmd_parts,
                    exit_code,
                    duration_ms,
                    cwd,
                    from_stdin,
                } => {
                    assert!(cmd_parts.is_empty());
                    assert_eq!(exit_code, 0);
                    assert_eq!(duration_ms, 100);
                    assert_eq!(cwd.as_deref(), Some("/tmp"));
                    assert!(from_stdin);
                }
                _ => panic!("expected record"),
            },
            _ => panic!("expected parse success"),
        }
    }

    #[test]
    fn parse_search_double_dash() {
        match parse_args(&["--".into(), "git commit".into()]) {
            Ok(cli) => match cli.action {
                Action::Search { initial_query } => {
                    assert_eq!(initial_query.as_deref(), Some("git commit"));
                }
                _ => panic!("expected search"),
            },
            _ => panic!("expected parse success"),
        }
    }

    #[test]
    fn parse_init_shells() {
        assert!(
            matches!(parse_args(&["init".into(), "bash".into()]), Ok(cli) if matches!(cli.action, Action::Init { ref shell } if shell == "bash"))
        );
        assert!(
            matches!(parse_args(&["init".into(), "zsh".into()]), Ok(cli) if matches!(cli.action, Action::Init { ref shell } if shell == "zsh"))
        );
        assert!(
            matches!(parse_args(&["init".into(), "fish".into()]), Ok(cli) if matches!(cli.action, Action::Init { ref shell } if shell == "fish"))
        );

        assert!(matches!(
            parse_args(&["init".into()]),
            Err(Error::InvalidArgs(_))
        ));
        assert!(matches!(
            parse_args(&["init".into(), "powershell".into()]),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn parse_history_subcommands() {
        assert!(
            matches!(parse_args(&["history".into(), "import".into()]), Ok(cli) if matches!(cli.action, Action::HistoryImport { path: None }))
        );
        assert!(
            matches!(parse_args(&["history".into(), "import".into(), "hist.log".into()]), Ok(cli) if matches!(cli.action, Action::HistoryImport { path: Some(ref p) } if p == "hist.log"))
        );
        assert!(
            matches!(parse_args(&["history".into(), "export".into()]), Ok(cli) if matches!(cli.action, Action::HistoryExport { path: None }))
        );
        assert!(
            matches!(parse_args(&["history".into(), "export".into(), "-".into()]), Ok(cli) if matches!(cli.action, Action::HistoryExport { path: Some(ref p) } if p == "-"))
        );
        assert!(
            matches!(parse_args(&["history".into(), "export".into(), "out.tsv".into()]), Ok(cli) if matches!(cli.action, Action::HistoryExport { path: Some(ref p) } if p == "out.tsv"))
        );

        assert!(matches!(
            parse_args(&["history".into()]),
            Err(Error::InvalidArgs(_))
        ));
        assert!(matches!(
            parse_args(&["history".into(), "import".into(), "--invalid".into()]),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn parse_global_flags_with_subcommand() {
        let args = vec![
            "-H".into(),
            "/tmp/custom.tsv".into(),
            "-s".into(),
            "local".into(),
            "history".into(),
            "export".into(),
        ];
        match parse_args(&args) {
            Ok(cli) => {
                assert_eq!(cli.history_path, Some(PathBuf::from("/tmp/custom.tsv")));
                assert_eq!(cli.scope, Some(arena::Scope::Local));
                assert!(matches!(cli.action, Action::HistoryExport { path: None }));
            }
            _ => panic!("expected parse success"),
        }
    }

    #[test]
    fn parse_unknown_subcommand() {
        assert!(
            matches!(parse_args(&["foobar".into()]), Err(Error::InvalidArgs(msg)) if msg == "unknown subcommand")
        );
        assert!(
            matches!(parse_args(&["--unknown-flag".into()]), Err(Error::InvalidArgs(msg)) if msg == "unknown option")
        );
    }

    #[test]
    fn parse_preview_flags() {
        match parse_args(&["--preview=fixed".into()]) {
            Ok(cli) => assert_eq!(cli.preview, Some(arena::PreviewMode::Fixed)),
            _ => panic!("expected parse success"),
        }
        match parse_args(&["--preview".into(), "dynamic".into()]) {
            Ok(cli) => assert_eq!(cli.preview, Some(arena::PreviewMode::Dynamic)),
            _ => panic!("expected parse success"),
        }
        match parse_args(&["-p".into(), "fixed".into()]) {
            Ok(cli) => assert_eq!(cli.preview, Some(arena::PreviewMode::Fixed)),
            _ => panic!("expected parse success"),
        }
        match parse_args(&["search".into(), "--preview=fixed".into()]) {
            Ok(cli) => assert_eq!(cli.preview, Some(arena::PreviewMode::Fixed)),
            _ => panic!("expected parse success"),
        }
        assert!(parse_args(&["--preview=unknown".into()]).is_err());
        assert!(parse_args(&["--preview".into()]).is_err());
    }

    #[test]
    fn shell_hooks_content() {
        assert!(BASH_HOOK.contains("__xstory_search"));
        assert!(ZSH_HOOK.contains("__xstory_widget"));
        assert!(FISH_HOOK.contains("__xstory_search"));
    }
}
