# Shell Integration Architecture Specification: `xstory init <shell>` & Hooks
> **Status:** APPROVED ARCHITECTURAL SPECIFICATION · **Standard:** Principal+ Systems & UX/UI Architecture  
> **Reference Peers:** `ripgrep`, `starship`, `atuin`, `zoxide`, `fd` · **Target Footprint:** Zero-Cost, Zero-Fork, Zero-Alloc (< 2000 LOC Budget)

---

## 1. Architectural Philosophy & Executive Invariants

Интеграция утилиты истории с командной оболочкой — критический путь системы. Любой дефект в shell-интеграции разрушает пользовательский опыт терминала: вызывает микрофризы при вводе, искажает вывод командных конвейеров, ломает `$?` в промпте или приводит к утечке конфиденциальных данных в системную таблицу процессов.

`xstory` проектируется как золотой стандарт CLI на Rust. Для подсистемы `init` и хуков жизненного цикла фиксируются 7 фундаментальных архитектурных инвариантов:

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│                        XSTORY SHELL INTEGRATION INVARIANTS                             │
├────────────────────────────────────────────────────────────────────────────────────────┤
│ 1. Zero-Fork Hot Path    │ В preexec/precmd запрещен spawn внешних утилит (date/python)│
│ 2. Process Table Safety  │ Команды передаются через STDIN (защита от ps aux / E2BIG)   │
│ 3. Strict FD Separation  │ STDOUT = Payload only; STDERR = TUI Screen; STDIN = TTY    │
│ 4. Kernel 6.2+ Immunity  │ Отказ от ioctl(TIOCSTI); 100% нативные макросы редакторов   │
│ 5. Atomic Status Capture │ $? и PIPESTATUS фиксируются до выполнения первой строки хука│
│ 6. Fail-Safe Degradation │ Сбой/отсутствие бинарника не ломает шелл и не спамит ошибки │
│ 7. Canonical TUI Invocation: Вызов TUI строго через POSIX-разделитель: xstory -- "$BUF"│
└────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 2. Low-Level Kernel & Shell Mechanics: Решение системных проблем

### 2.1. Измерение длительности (Duration) и захват команды без внешних процессов

#### Патология наивных реализаций
Попытка вызова `date +%s%3N`, `python3` или `$(HISTTIMEFORMAT= history 1)` в теле перехватчика:
1. **Каскадный fork/exec:** На macOS запуск `python3` занимает **25–60 мс**, а `/bin/date` — **2–5 мс**. Два вызова на каждую команду добавляют до **120 мс задержки** к каждому нажатию Enter.
2. **BSD vs GNU несовместимость:** Стандартная утилита `date` в macOS (BSD) **не поддерживает флаг `%N`** и возвращает литерал `...3N`, ломая последующую арифметику шелла синтаксической ошибкой.
3. **Subshell fork при захвате истории в Bash:** Вызов `$(history 1)` создает subshell-процесс на каждый Enter, а при пустом нажатии Enter возвращает старую команду и повторно перезаписывает ее.

#### Principal-решения по шеллам:
* **Zsh:** Встроенный модуль `zmodload zsh/datetime 2>/dev/null` предоставляет `$EPOCHREALTIME` с микросекундной точностью. Дельта вычисляется через нативную арифметику Zsh: `dur_ms=$(( (EPOCHREALTIME - start_time) * 1000 ))`. Аргумент команды передается в `preexec` напрямую через `$1`.
* **Fish:** C++ event loop Fish нативно замеряет время через `std::chrono::steady_clock` и экспортирует готовую переменную `$CMD_DURATION` (миллисекунды). Команда передается в `fish_postexec` через `$argv[1]`.
* **Bash:** В `DEBUG` trap перехватывается переменная `$BASH_COMMAND` без единого вызова `history 1` и без subshell. Индекс `HISTCMD` отслеживает изменения: при пустом вводе Enter `HISTCMD` не увеличивается, и хук мгновенно завершается. Замер времени — через `$EPOCHREALTIME` (Bash 5.0+) со стрип-трюком `${var//[!0-9]}` или через целочисленный `$SECONDS` (Bash 3.2/4).

---

### 2.2. Атомарный перехват `$?` и массивов конвейеров (`PIPESTATUS`)

Шеллы сбрасывают `$?` и статус конвейера при выполнении любой встроенной команды. Например, `local exit_code=$?` немедленно перезаписывает `${PIPESTATUS[@]}` кодом `(0)`.

```mermaid
sequenceDiagram
    autonumber
    participant Pipeline as User Pipeline (cmd1 | cmd2)
    participant Shell as Shell Runtime
    participant Hook as xstory_precmd Hook

    Pipeline->>Shell: Execution finished ($?=1, PIPESTATUS=(0, 1))
    Note over Shell,Hook: Опасная зона: любая команда сбросит PIPESTATUS!
    Shell->>Hook: PROMPT_COMMAND='__xstory_precmd "$?" "${PIPESTATUS[@]}"'
    Note over Hook: Аргументы раскрыты шеллом АТОМАРНО до входа в функцию
    Hook->>Hook: local last_exit=$1; shift; local -a last_pipe=("$@")
    Note over Hook: Статусы гарантированно сохранены в стеке
```

* **Bash:** Регистрация в `PROMPT_COMMAND` в виде вызова с параметрами:  
  `PROMPT_COMMAND='__xstory_precmd "$?" "${PIPESTATUS[@]}"; '"${PROMPT_COMMAND-}"`
* **Zsh:** Атомарное множественное присваивание в одном выражении:  
  `local last_exit=$? last_pipe=("${pipestatus[@]}")`
* **Fish:** Переменные `$status` и `$pipestatus` сохраняются в первых строках `fish_postexec`:  
  `set -l last_status $status; set -l last_pipestatus $pipestatus`

---

### 2.3. Изоляция файловых дескрипторов и TTY-сепарация

При интерактивном поиске по `Ctrl-R` шелл запускает утилиту внутри подстановки команд. Согласно каноническому CLI-контракту `xstory [OPTIONS] [QUERY] [COMMAND]`, вызов производится строго через позиционный аргумент с разделителем `--`:
* **Bash:** `selected=$(xstory -- "$READLINE_LINE")`
* **Zsh:** `selected=$(xstory -- "$BUFFER")`
* **Fish:** `set -l selected (xstory -- (commandline -b) | string collect)`

Разделитель `--` предотвращает сбои парсера, если текущий буфер начинается с дефиса (`-v`, `--help`) или совпадает с зарезервированными подкомандами (`record`, `init`). Фильтр `string collect` в Fish гарантирует целостность многострочных команд, предотвращая их расщепление на массив строк по символу перевода строки `\n`.

```
┌────────────────────────────────────────────────────────────────────────┐
│                      SUB-SHELL FD ROUTING MATRIX                       │
├───────────────┬─────────────────────────────┬──────────────────────────┤
│ FD Descriptor │ Назначение                  │ Физическая привязка      │
├───────────────┼─────────────────────────────┼──────────────────────────┤
│ STDOUT (1)    │ Payload (выбранная команда) │ Pipe -> Родительский шелл│
│ STDERR (2)    │ TUI Screen (Ratatui/ANSI)   │ Псевдотерминал (/dev/tty)│
│ STDIN (0)     │ Keyboard Input (Crossterm)  │ /dev/tty                 │
└───────────────┴─────────────────────────────┴──────────────────────────┘
```

> [!IMPORTANT]
> Если TUI-библиотека пытается писать в STDOUT, управляющие ANSI-последовательности попадают в переменную `selected`, вызывая зависание экрана и порчу буфера. В `xstory` рендеринг жестко закреплен за STDERR, а STDOUT активируется только после полного отключения Raw Mode для однократного сброса чистого текста выбранной команды.

---

### 2.4. Обход блокировки `ioctl(TIOCSTI)` в ядрах Linux 6.2+

Начиная с Linux 6.2 параметр `dev.tty.legacy_tiocsti=0` блокирует `ioctl(TIOCSTI)` (ошибка `-1 EIO/EPERM`).

#### Двухшаговый макро-диспатчинг в Bash (Two-Step Readline Macro)
1. Пользователь нажимает `Ctrl-R`.
2. Readline разворачивает макрос в очередь: `[\C-x1, \C-x2]`.
3. Readline вызывает `\C-x1` (зарегистрированный через `bind -x`). Запускается функция `__xstory_search_dispatch`.
4. Функция выполняет поиск и анализирует код завершения:
   - **Exit 0 (Execute):** `READLINE_LINE="$cmd"`; динамически настраивает `bind '"\C-x2": accept-line'`.
   - **Exit 2 (Edit):** `READLINE_LINE="$cmd"`; динамически настраивает `bind '"\C-x2": ""'` (no-op).
   - **Exit 130 (Cancel):** буфер не изменяется; настраивает `bind '"\C-x2": ""'` (no-op).
5. Readline завершает шаг 1 и немедленно берет из очереди `\C-x2`. Если установлена `accept-line`, команда исполняется мгновенно!

---

## 3. Безопасность: передача команд через `--stdin`

Передача открытого текста команды через аргументы CLI (`xstory record "$cmd"`) раскрывает конфиденциальные данные (токены, пароли) в `/proc/$PID/cmdline` и `ps aux`, а также упирается в ограничение ядра `MAX_ARG_STRLEN` (128 КБ, `E2BIG`).

**Стандарт `xstory`:**
```bash
printf '%s' "$cmd" | xstory record --stdin --exit="$exit" --duration="$dur" --cwd="$PWD"
```
Команда передается напрямую в анонимный дескриптор в память `xstory`, исключая попадание в таблицу процессов ОС.

---

## 4. Эталонные реализации Shell Hooks

### 4.1. Zsh Reference Implementation (`xstory init zsh`)

```zsh
# ── xstory integration for zsh ───────────────────────────────────────────
[[ -o interactive ]] || return 0
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
    local last_exit=$? last_pipe=("${pipestatus[@]}")

    if [[ -n "$__xstory_cmd" ]]; then
        if [[ "$__xstory_cmd" != [[:space:]]* ]]; then
            local -F now=$EPOCHREALTIME
            local -i dur_ms=$(( (now - __xstory_start_time) * 1000 ))
            (( dur_ms < 0 )) && dur_ms=0

            print -rn -- "$__xstory_cmd" | xstory record \
                --stdin \
                --exit="$last_exit" \
                --duration="$dur_ms" \
                --cwd="$PWD" >/dev/null 2>&1
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
bindkey -M viins '^R' __xstory_widget
bindkey -M vicmd '^R' __xstory_widget

if [[ -n "${ZVM_VERSION-}" ]]; then
    zvm_after_init_commands+=(
        "bindkey -M viins '^R' __xstory_widget"
        "bindkey -M vicmd '^R' __xstory_widget"
    )
fi
```

---

### 4.2. Bash Reference Implementation (`xstory init bash`)

```bash
# ── xstory integration for bash ──────────────────────────────────────────
[[ $- == *i* ]] || return 0
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
        local last_exit=$1
        shift
        local -a last_pipe=("$@")

        # Отсечение пустого Enter: HISTCMD не изменился
        if (( HISTCMD <= __xstory_last_histcmd )); then
            __xstory_cmd=""
            unset __xstory_start_time
            return 0
        fi
        __xstory_last_histcmd=$HISTCMD

        if [[ -n "${__xstory_cmd-}" && -n "${__xstory_start_time-}" ]]; then
            local dur_ms=0
            if (( BASH_VERSINFO[0] >= 5 )) && [[ -n "${EPOCHREALTIME-}" ]]; then
                local start_us=${__xstory_start_time//[!0-9]}
                local now_us=${EPOCHREALTIME//[!0-9]}
                dur_ms=$(( (now_us - start_us) / 1000 ))
                (( dur_ms < 0 )) && dur_ms=0
            else
                local dur_s=$(( SECONDS - __xstory_start_time ))
                (( dur_s < 0 )) && dur_s=0
                dur_ms=$(( dur_s * 1000 ))
            fi

            if [[ "$__xstory_cmd" != [[:space:]]* && "$__xstory_cmd" != "__xstory"* ]]; then
                printf '%s' "$__xstory_cmd" | xstory record \
                    --stdin \
                    --exit="$last_exit" \
                    --duration="$dur_ms" \
                    --cwd="$PWD" >/dev/null 2>&1
            fi
            __xstory_cmd=""
            unset __xstory_start_time
        fi
    }

    if [[ -n "${BLE_VERSION-}" ]]; then
        blehook PREEXEC!='__xstory_preexec'
        blehook PRECMD!='__xstory_precmd "$?" "${PIPESTATUS[@]}"'
    elif [[ -n "${bash_preexec_imported-}" || -n "${__bp_imported-}" ]]; then
        preexec_functions+=(__xstory_preexec)
        precmd_functions+=(__xstory_precmd_wrapper)
        __xstory_precmd_wrapper() { __xstory_precmd "$?" "${PIPESTATUS[@]}"; }
    else
        trap '__xstory_preexec' DEBUG
        if [[ -z "${PROMPT_COMMAND-}" ]]; then
            PROMPT_COMMAND='__xstory_precmd "$?" "${PIPESTATUS[@]}"'
        elif [[ "$PROMPT_COMMAND" != *"__xstory_precmd"* ]]; then
            PROMPT_COMMAND='__xstory_precmd "$?" "${PIPESTATUS[@]}"; '"$PROMPT_COMMAND"
        fi
    fi

    local KEY_STEP1='\C-x1'
    local KEY_STEP2='\C-x2'

    __xstory_search_dispatch() {
        local selected ret
        selected=$(xstory -- "$READLINE_LINE")
        ret=$?

        if [[ $ret -eq 0 && -n "$selected" ]]; then
            READLINE_LINE="$selected"
            READLINE_POINT=${#READLINE_LINE}
            bind "\"$KEY_STEP2\": accept-line"
        elif [[ $ret -eq 2 && -n "$selected" ]]; then
            READLINE_LINE="$selected"
            READLINE_POINT=${#READLINE_LINE}
            bind "\"$KEY_STEP2\": \"\""
        else
            bind "\"$KEY_STEP2\": \"\""
        fi
    }

    bind -m emacs -x "\"$KEY_STEP1\": __xstory_search_dispatch"
    bind -m vi-insert -x "\"$KEY_STEP1\": __xstory_search_dispatch"
    bind -m emacs "\"$KEY_STEP2\": \"\""
    bind -m vi-insert "\"$KEY_STEP2\": \"\""

    bind -m emacs "\"\\C-r\": \"$KEY_STEP1$KEY_STEP2\""
    bind -m vi-insert "\"\\C-r\": \"$KEY_STEP1$KEY_STEP2\""
}
__xstory_init
unset -f __xstory_init
```

---

### 4.3. Fish Reference Implementation (`xstory init fish`)

```fish
# ── xstory integration for fish ──────────────────────────────────────────
status is-interactive; or exit 0
set -q XSTORY_DISABLE; and exit 0
command -q xstory; or exit 0

function __xstory_postexec --on-event fish_postexec
    if set -q fish_private_mode
        return
    end

    if string match -qr '^\s' -- "$argv[1]"
        return
    end

    set -l last_status $status
    set -l last_pipestatus $pipestatus
    set -l dur_ms 0
    test -n "$CMD_DURATION"; and set dur_ms $CMD_DURATION

    printf '%s' "$argv[1]" | xstory record \
        --stdin \
        --exit="$last_status" \
        --duration="$dur_ms" \
        --cwd="$PWD" >/dev/null 2>&1
end

function __xstory_search
    set -l current_buf (commandline -b)
    set -l selected (xstory -- "$current_buf" | string collect)
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

bind \cr __xstory_search
if bind -M insert >/dev/null 2>&1
    bind -M insert \cr __xstory_search
end
```

---

## 5. CLI Контракт и бюджет кода

Команда `init` жестко ограничена тремя поддерживаемыми Unix-оболочками:
```
xstory init <bash|zsh|fish>
```
Никаких избыточных runtime-флагов, внешних шаблонизаторов или сканирования `$PATH`. Вызов бинарника `xstory` делегируется системному PATH hash table оболочки.
Общий бюджет кодовой базы сохраняется строго в пределах **< 2000 LOC**.
