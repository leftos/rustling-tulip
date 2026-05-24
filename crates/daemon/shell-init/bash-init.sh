# shellcheck shell=bash
# rustling-tulip shell-integration init for bash.
#
# Loaded via `bash --rcfile <this-file> -i` from the daemon. Sources the
# user's real ~/.bashrc first so their setup wins, then layers OSC 7
# (cwd) + OSC 133 (prompt/command/output marks) + OSC 633 (command line
# payload) hooks on top. The frontend parses these to render per-command
# chips with copy + re-run actions.
#
# Re-entry is guarded by __rt_busy so the DEBUG trap fires only for
# top-level user commands, not for our own prompt-command statements.

if [[ -r "$HOME/.bashrc" ]]; then
    # shellcheck disable=SC1091
    builtin source "$HOME/.bashrc"
fi

__rt_busy=""
__rt_in_command=""

__rt_emit_cwd() {
    builtin printf '\e]7;file://%s%s\a' "${HOSTNAME:-}" "$PWD"
}

__rt_preexec() {
    [[ -n "$__rt_busy" || -n "$__rt_in_command" ]] && return
    [[ -n "$COMP_LINE" ]] && return
    __rt_busy="1"
    __rt_in_command="1"
    local cmd="$BASH_COMMAND"
    local escaped="${cmd//\\/\\\\}"
    escaped="${escaped//;/\\x3b}"
    escaped="${escaped//$'\n'/\\x0a}"
    builtin printf '\e]633;E;%s\a' "$escaped"
    builtin printf '\e]133;C\a'
    __rt_busy=""
}

__rt_precmd() {
    local exit=$?
    __rt_busy="1"
    if [[ -n "$__rt_in_command" ]]; then
        builtin printf '\e]133;D;%s\a' "$exit"
        __rt_in_command=""
    fi
    __rt_emit_cwd
    builtin printf '\e]133;A\a'
    __rt_busy=""
}

if [[ -z "$PROMPT_COMMAND" ]]; then
    PROMPT_COMMAND='__rt_precmd'
else
    PROMPT_COMMAND='__rt_precmd; '"$PROMPT_COMMAND"
fi

trap '__rt_preexec' DEBUG
