#!/usr/bin/env bash
# Desktop launch helper: respect the selected terminal instead of Terminal=true's
# desktop-specific fallback. All command/file arguments remain separate argv items.
set -euo pipefail
SCRIPT_DIR=$(cd -- "${BASH_SOURCE[0]%/*}" && pwd -P)
BIN="$SCRIPT_DIR/deb2rpm"
PACKAGE_ARGS=("$@")

launch() {
    local terminal=$1
    command -v -- "$terminal" >/dev/null 2>&1 || return 1
    local -a args
    case "${terminal##*/}" in
        kitty|foot|footclient|gnome-terminal|kgx) args=(--) ;;
        wezterm) args=(start --) ;;
        ptyxis) args=(--new-window --) ;;
        xfce4-terminal|mate-terminal) args=(-x) ;;
        alacritty|ghostty|konsole|xterm|urxvt|st|tilix|x-terminal-emulator) args=(-e) ;;
        *) return 1 ;;
    esac
    exec "$terminal" "${args[@]}" "$BIN" "${PACKAGE_ARGS[@]}"
}

if [[ -n ${TERMINAL:-} ]]; then
    if ! launch "$TERMINAL"; then
        printf 'deb2rpm: cannot launch default terminal %s; install xdg-terminal-exec or set TERMINAL to a supported terminal executable.\n' "$TERMINAL" >&2
        exit 1
    fi
fi

if command -v xdg-terminal-exec >/dev/null 2>&1; then
    exec xdg-terminal-exec "$BIN" "${PACKAGE_ARGS[@]}"
fi

# Compatibility for systems without xdg-terminal-exec. Read explicit XDG
# preferences for common terminals; do not guess from installed applications.
config_home=${XDG_CONFIG_HOME:-$HOME/.config}
[[ $config_home = /* ]] || config_home="$HOME/.config"
IFS=: read -r -a config_dirs <<< "${XDG_CONFIG_DIRS:-/etc/xdg}"
config_dirs=("$config_home" "${config_dirs[@]}")
IFS=: read -r -a desktops <<< "${XDG_CURRENT_DESKTOP:-}"
declare -A excluded=()
for directory in "${config_dirs[@]}"; do
    [[ $directory = /* ]] || continue
    lists=()
    for desktop in "${desktops[@]}"; do
        [[ -n $desktop ]] && lists+=("$directory/${desktop,,}-xdg-terminals.list")
    done
    lists+=("$directory/xdg-terminals.list")
    for list in "${lists[@]}"; do
        [[ -f $list ]] || continue
        while IFS= read -r entry || [[ -n $entry ]]; do
            entry=${entry%%#*}
            entry=${entry#"${entry%%[![:space:]]*}"}
            entry=${entry%"${entry##*[![:space:]]}"}
            [[ -n $entry ]] || continue
            case "$entry" in
                -*) excluded[${entry#-}]=1; continue ;;
                +*) key=${entry#+}; unset 'excluded[$key]'; continue ;;
                /*) continue ;;
            esac
            [[ -z ${excluded[$entry]:-} ]] || continue
            case "$entry" in
                kitty.desktop) terminal=kitty ;;
                Alacritty.desktop|alacritty.desktop|org.alacritty.Alacritty.desktop) terminal=alacritty ;;
                foot.desktop|org.codeberg.dnkl.foot.desktop) terminal=foot ;;
                footclient.desktop) terminal=footclient ;;
                org.wezfurlong.wezterm.desktop) terminal=wezterm ;;
                com.mitchellh.ghostty.desktop) terminal=ghostty ;;
                org.kde.konsole.desktop) terminal=konsole ;;
                org.gnome.Terminal.desktop) terminal=gnome-terminal ;;
                org.gnome.Ptyxis.desktop) terminal=ptyxis ;;
                org.gnome.Console.desktop) terminal=kgx ;;
                xfce4-terminal.desktop) terminal=xfce4-terminal ;;
                mate-terminal.desktop) terminal=mate-terminal ;;
                xterm.desktop) terminal=xterm ;;
                *) continue ;;
            esac
            launch "$terminal" || continue
        done < "$list"
    done
done
if command -v x-terminal-emulator >/dev/null 2>&1; then
    launch x-terminal-emulator
fi
printf 'deb2rpm: no supported default terminal found. Install xdg-terminal-exec, set TERMINAL, or select a terminal in ~/.config/xdg-terminals.list.\n' >&2
exit 1
