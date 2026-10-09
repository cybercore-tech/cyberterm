# Cyberterm shell integration for bash (4.4+).
#
#   eval "$(cyberterm +shell-integration bash)"    # in ~/.bashrc
#
# Reports the working directory (OSC 7) and marks each prompt and command
# (OSC 133) so Cyberterm can jump between prompts and remember exit codes.

[[ $- == *i* ]] || return 0
[[ -n ${_CYBERTERM_INTEGRATED-} ]] && return 0
_CYBERTERM_INTEGRATED=1
_cyberterm_running=0

_cyberterm_urlencode() {
  local LC_ALL=C s=$1 out= c i
  for (( i = 0; i < ${#s}; i++ )); do
    c=${s:i:1}
    case $c in
      [A-Za-z0-9/._~-]) out+=$c ;;
      *) printf -v c '%%%02X' "'$c"; out+=$c ;;
    esac
  done
  REPLY=$out
}

_cyberterm_prompt_command() {
  local ret=$?
  if (( _cyberterm_running )); then
    printf '\e]133;D;%d\a' "$ret"
    _cyberterm_running=0
  fi
  _cyberterm_urlencode "$PWD"
  printf '\e]7;file://%s%s\a' "${HOSTNAME}" "$REPLY"
  if [[ $PS1 != *'133;A'* ]]; then
    PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'
  fi
  return $ret
}

# The command about to run (history's newest entry), percent-encoded.
_cyberterm_cmdline() {
  local c
  c=$(HISTTIMEFORMAT= builtin history 1)
  c=${c#*[0-9]  }
  _cyberterm_urlencode "$c"
  printf '%s' "$REPLY"
}

# PS0 is expanded after a command line is read, right before it runs. The
# array-subscript arithmetic sets the flag in this shell (not a subshell)
# while expanding to nothing; the command line rides along on 133;C.
PS0='${_cyberterm_noop[$((_cyberterm_running=1))]}\e]133;C;cmdline_url=$(_cyberterm_cmdline)\a'"${PS0-}"
PROMPT_COMMAND="_cyberterm_prompt_command${PROMPT_COMMAND:+;$PROMPT_COMMAND}"
