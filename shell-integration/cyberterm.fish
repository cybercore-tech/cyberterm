# Cyberterm shell integration for fish.
#
#   cyberterm +shell-integration fish | source    # in ~/.config/fish/config.fish
#
# Reports the working directory (OSC 7) and marks each prompt and command
# (OSC 133) so Cyberterm can jump between prompts and remember exit codes.

status is-interactive; or exit 0
set -q _CYBERTERM_INTEGRATED; and exit 0
set -g _CYBERTERM_INTEGRATED 1

function __cyberterm_report_pwd --on-variable PWD
    printf '\e]7;file://%s%s\a' (hostname) (string escape --style=url -- $PWD)
end
__cyberterm_report_pwd

function __cyberterm_preexec --on-event fish_preexec
    printf '\e]133;C\a'
end

function __cyberterm_postexec --on-event fish_postexec
    printf '\e]133;D;%d\a' $status
end

functions -q fish_prompt; and functions -c fish_prompt __cyberterm_original_prompt
function fish_prompt
    printf '\e]133;A\a'
    if functions -q __cyberterm_original_prompt
        __cyberterm_original_prompt
    end
    printf '\e]133;B\a'
end
