# artty shell integration for zsh: marks prompts and commands (OSC 133) and
# reports the working directory (OSC 7). Loaded automatically by artty; to
# load it yourself, source this file at the end of ~/.zshrc.
[[ -o interactive ]] || return 0
(( ${+_ARTTY_INTEGRATED} )) && return 0
typeset -g _ARTTY_INTEGRATED=1 _artty_running=0

_artty_precmd() {
  local ret=$?
  (( _artty_running )) && printf '\e]133;D;%d\a' $ret
  _artty_running=0
  printf '\e]7;file://%s%s\a' "$HOST" "${PWD// /%20}"
  # Prompt themes rebuild PS1 in their own hooks, so mark it here and stay
  # the last hook to run.
  [[ $PS1 == *$'\e]133;A'* ]] || PS1=$'%{\e]133;A\a%}'$PS1$'%{\e]133;B\a%}'
  precmd_functions=(${precmd_functions:#_artty_precmd} _artty_precmd)
}

_artty_preexec() {
  _artty_running=1
  printf '\e]133;C\a'
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd _artty_precmd
add-zsh-hook preexec _artty_preexec
