# Cyberterm shell integration for zsh.
#
#   eval "$(cyberterm +shell-integration zsh)"    # in ~/.zshrc
#
# Reports the working directory (OSC 7) and marks each prompt and command
# (OSC 133), which is what lets Cyberterm jump between prompts
# (Ctrl+Shift+Z / Ctrl+Shift+X) and remember exit codes. Harmless in other
# terminals: unknown OSC sequences are ignored.

[[ -o interactive ]] || return 0
(( ${+_CYBERTERM_INTEGRATED} )) && return 0
typeset -g _CYBERTERM_INTEGRATED=1
typeset -gi _cyberterm_running=0

_cyberterm_urlencode() {
  emulate -L zsh
  local LC_ALL=C s=$1 out= c i
  for (( i = 1; i <= ${#s}; i++ )); do
    c=${s[i]}
    case $c in
      [A-Za-z0-9/._~-]) out+=$c ;;
      *) printf -v c '%%%02X' "'$c"; out+=$c ;;
    esac
  done
  REPLY=$out
}

_cyberterm_precmd() {
  local ret=$?
  if (( _cyberterm_running )); then
    printf '\e]133;D;%d\a' "$ret"
    _cyberterm_running=0
  fi
  _cyberterm_urlencode "$PWD"
  printf '\e]7;file://%s%s\a' "${HOST}" "$REPLY"
  # Wrap the prompt last, after themes (starship, p10k, ...) have set it.
  # The marks sit inside PS1 so prompt redraws (resizes, reset-prompt)
  # re-mark it too.
  if [[ $PS1 != *$'\e]133;A'* ]]; then
    PS1=$'%{\e]133;A\a%}'"$PS1"$'%{\e]133;B\a%}'
  fi
}

_cyberterm_preexec() {
  printf '\e]133;C\a'
  _cyberterm_running=1
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _cyberterm_preexec
# Run after every other precmd hook so their PS1 changes get wrapped.
_cyberterm_late_precmd() {
  precmd_functions=(${precmd_functions:#_cyberterm_precmd} _cyberterm_precmd)
}
add-zsh-hook precmd _cyberterm_late_precmd
add-zsh-hook precmd _cyberterm_precmd
