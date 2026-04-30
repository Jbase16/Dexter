# Dexter shell integration for zsh.
#
# Installation — add to ~/.zshrc:
#   export DEXTER_ROOT="$HOME/Developer/Dex"   # adjust to your checkout path
#   source "$DEXTER_ROOT/config/shell_integration.zsh"
#
# After each command completes, sends a JSON notification to the Dexter core
# over a Unix domain socket. Dexter stores the command, working directory, and
# exit code as shell context, injecting it into inference as:
#
#   [Shell: $ <command> → exit <N> in <cwd>]
#
# Fire-and-forget: the notification runs in a background subprocess; if Dexter
# is not running, nc fails silently. Zero prompt latency impact.

# Socket path. Override with DEXTER_SHELL_SOCKET for testing.
_dexter_socket="${DEXTER_SHELL_SOCKET:-/tmp/dexter-shell.sock}"

# Stores the command string captured in preexec, consumed in precmd.
_dexter_last_cmd=""

# preexec: called by zsh before each command line executes.
# $1 is the raw command string as typed by the operator.
_dexter_preexec() {
    _dexter_last_cmd="$1"
}

# precmd: called by zsh after each command completes.
# $? is the exit code of the just-completed command.
_dexter_precmd() {
    local _exit="$?"
    [[ -z "$_dexter_last_cmd" ]] && return   # nothing was run (e.g. bare Enter)
    local _cmd="$_dexter_last_cmd"
    local _cwd="$PWD"    # CWD after the command — captures `cd` destination correctly
    _dexter_last_cmd=""  # clear so a bare Enter in subsequent precmd doesn't resend

    # python3 handles all JSON escaping (quotes, backslashes, Unicode, control chars).
    # nc -U writes to the Unix socket and exits when the pipe closes.
    # (...) &! = subshell, background, disown — no "Done:" completion messages in zsh.
    (python3 -c "
import json, sys
payload = json.dumps({
    'command':   sys.argv[1],
    'cwd':       sys.argv[2],
    'exit_code': int(sys.argv[3]),
})
print(payload, end='')
" "$_cmd" "$_cwd" "$_exit" | nc -U "$_dexter_socket" 2>/dev/null) &!
}

# Register with zsh's hook system.
# autoload is required before add-zsh-hook.
# add-zsh-hook is idempotent — safe to source multiple times.
autoload -Uz add-zsh-hook
add-zsh-hook preexec _dexter_preexec
add-zsh-hook precmd  _dexter_precmd
