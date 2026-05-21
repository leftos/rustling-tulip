# rustling-tulip shell-integration init for zsh.
#
# Loaded by setting ZDOTDIR to the directory containing this file. The
# daemon stashes the user's original ZDOTDIR in
# $RUSTLING_TULIP_ZSH_USER_ZDOTDIR so we can source the real .zshrc first,
# then layer OSC 7 (cwd) + OSC 133 (prompt/command/output marks) + OSC 633
# (command line payload) hooks on top. The frontend parses these to render
# per-command chips with copy + re-run actions.

__rt_user_zdotdir="${RUSTLING_TULIP_ZSH_USER_ZDOTDIR:-$HOME}"

if [[ -r "$__rt_user_zdotdir/.zshrc" ]]; then
    # shellcheck disable=SC1090
    source "$__rt_user_zdotdir/.zshrc"
fi

autoload -Uz add-zsh-hook

__rt_emit_cwd() {
    printf '\e]7;file://%s%s\a' "${HOST:-}" "$PWD"
}

__rt_preexec() {
    local cmd="$1"
    local escaped="${cmd//\\/\\\\}"
    escaped="${escaped//;/\\x3b}"
    escaped="${escaped//$'\n'/\\x0a}"
    printf '\e]633;E;%s\a' "$escaped"
    printf '\e]133;C\a'
}

__rt_precmd() {
    local exit=$?
    printf '\e]133;D;%s\a' "$exit"
    __rt_emit_cwd
    printf '\e]133;A\a'
}

add-zsh-hook preexec __rt_preexec
add-zsh-hook precmd __rt_precmd
