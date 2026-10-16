# artty shell integration for bash (3.2 and later): marks prompts and commands
# (OSC 133) and reports the working directory (OSC 7). Loaded automatically by
# artty; to load it yourself, source this file at the end of ~/.bashrc.

# artty starts bash under the name `sh` with ENV pointing here, so this runs
# before any startup file: leave POSIX mode and load the user's files as bash
# would.
if [[ -n $ARTTY_BASH_INJECT ]]; then
  unset ARTTY_BASH_INJECT ENV
  set +o posix
  # Bash started as `sh` turns POSIX mode back on after this file; undo that
  # at the first prompt.
  _artty_leave_posix=1
  if [[ -n $ARTTY_BASH_LOGIN ]]; then
    unset ARTTY_BASH_LOGIN
    [[ -r /etc/profile ]] && . /etc/profile
    for _artty_file in ~/.bash_profile ~/.bash_login ~/.profile; do
      [[ -r $_artty_file ]] && { . "$_artty_file"; break; }
    done
    unset _artty_file
  elif [[ -r ~/.bashrc ]]; then
    . ~/.bashrc
  fi
fi

[[ $- == *i* ]] || return 0
[[ -n $_ARTTY_INTEGRATED ]] && return 0
_ARTTY_INTEGRATED=1
_artty_running=
_artty_at_prompt=

_artty_prompt() {
  [[ -n $_artty_leave_posix ]] && { set +o posix; unset _artty_leave_posix; }
  [[ -n $_artty_running ]] && printf '\e]133;D;%d\a' "$_artty_status"
  _artty_running=
  printf '\e]7;file://%s%s\a' "$HOSTNAME" "${PWD// /%20}"
  [[ $PS1 == *'133;A'* ]] || PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'
  _artty_at_prompt=1
}

# Runs before every simple command; only the first one after a prompt is the
# user's command line.
_artty_preexec() {
  [[ -n $_artty_at_prompt && -z $COMP_LINE ]] || return 0
  _artty_at_prompt=
  _artty_running=1
  printf '\e]133;C\a'
}

trap '_artty_preexec' DEBUG
PROMPT_COMMAND='_artty_status=$?;'"${PROMPT_COMMAND:+$PROMPT_COMMAND;}"'_artty_prompt'
