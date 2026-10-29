#!/bin/sh
# Installs the latest Agent Context Monitor release on macOS or Linux.
#
#   curl -fsSL https://raw.githubusercontent.com/bradtraversy/agent-context-monitor/main/install.sh | sh
#
# On Linux it uses the .deb or .rpm when your system has apt or dnf/zypper,
# otherwise the AppImage. Set ACM_FORMAT=deb, rpm, or appimage to choose.
# Set ACM_VERSION=v0.1.1 to install a specific release instead of the latest.

set -eu

REPO="bradtraversy/agent-context-monitor"
APP_NAME="Agent Context Monitor"

say() { printf '%s\n' "$*"; }
fail() { printf 'Error: %s\n' "$*" >&2; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }

has curl || fail "curl is needed to download the app."

if [ "$(id -u)" -eq 0 ]; then
  SUDO=""
elif has sudo; then
  SUDO="sudo"
else
  SUDO=""
fi

tmp=$(mktemp -d)
cleanup() {
  if [ -n "${mnt:-}" ] && [ -d "$mnt" ]; then hdiutil detach "$mnt" -quiet 2>/dev/null || true; fi
  rm -rf "$tmp"
}
trap cleanup EXIT INT TERM

if [ -n "${ACM_VERSION:-}" ]; then
  api="https://api.github.com/repos/$REPO/releases/tags/$ACM_VERSION"
else
  api="https://api.github.com/repos/$REPO/releases/latest"
fi

say "Checking for the latest release..."
# A token is optional. It only raises GitHub's rate limit, which CI runners share.
if [ -n "${ACM_GITHUB_TOKEN:-}" ]; then
  set -- -H "Authorization: Bearer $ACM_GITHUB_TOKEN"
else
  set --
fi
status=$(curl -sSL -H "Accept: application/vnd.github+json" "$@" "$api" -o "$tmp/release.json" -w '%{http_code}') \
  || fail "couldn't reach GitHub. Check your connection and try again."
case "$status" in
  200) ;;
  404)
    if [ -n "${ACM_VERSION:-}" ]; then
      fail "there's no release called $ACM_VERSION."
    else
      fail "there's no published release yet."
    fi
    ;;
  403 | 429) fail "GitHub's rate limit was hit. Wait a few minutes and try again." ;;
  *) fail "GitHub answered with HTTP $status. Try again in a moment." ;;
esac

tag=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp/release.json" | head -n 1)
# The download links for every file attached to the release, one per line.
urls=$(grep -o '"browser_download_url": *"[^"]*"' "$tmp/release.json" | sed 's/.*"\(https:[^"]*\)"/\1/')

# Prints the first download link ending in the given suffix.
asset() {
  printf '%s\n' "$urls" | grep -- "$1\$" | head -n 1
}

download() {
  say "Downloading $(basename "$1")..."
  curl -fL --progress-bar "$1" -o "$2" || fail "the download failed."
}

install_mac() {
  url=$(asset "_universal.dmg")
  [ -n "$url" ] || url=$(asset ".dmg")
  [ -n "$url" ] || fail "no Mac download found in release $tag."

  download "$url" "$tmp/acm.dmg"

  mnt="$tmp/mnt"
  mkdir -p "$mnt"
  hdiutil attach "$tmp/acm.dmg" -nobrowse -readonly -quiet -mountpoint "$mnt" \
    || fail "couldn't open the disk image."

  src="$mnt/$APP_NAME.app"
  [ -d "$src" ] || fail "the disk image doesn't contain $APP_NAME.app."

  dest_dir="/Applications"
  if [ ! -w "$dest_dir" ] && [ -z "$SUDO" ]; then
    dest_dir="$HOME/Applications"
    mkdir -p "$dest_dir"
  fi
  dest="$dest_dir/$APP_NAME.app"

  # Quit a running copy so it can be replaced.
  osascript -e "quit app \"$APP_NAME\"" >/dev/null 2>&1 || true

  say "Installing to $dest_dir..."
  if [ -w "$dest_dir" ]; then
    rm -rf "$dest"
    ditto "$src" "$dest"
  else
    $SUDO rm -rf "$dest"
    $SUDO ditto "$src" "$dest"
  fi

  hdiutil detach "$mnt" -quiet || true
  mnt=""

  open "$dest" || true
  say ""
  say "$APP_NAME $tag is installed and running. Look for it in your menu bar."
}

install_linux() {
  case "$(uname -m)" in
    x86_64 | amd64) ;;
    *) fail "only 64-bit Intel/AMD Linux (x86_64) is supported for now, this machine is $(uname -m)." ;;
  esac

  format="${ACM_FORMAT:-}"
  if [ -z "$format" ]; then
    if has apt-get && has dpkg; then
      format="deb"
    elif has dnf || has zypper; then
      format="rpm"
    else
      format="appimage"
    fi
  fi

  # Installing a package needs root. Without sudo, fall back to the AppImage.
  if [ "$format" != "appimage" ] && [ "$(id -u)" -ne 0 ] && [ -z "$SUDO" ]; then
    say "sudo isn't available, so installing the AppImage instead."
    format="appimage"
  fi

  case "$format" in
    deb)
      url=$(asset "_amd64.deb")
      [ -n "$url" ] || fail "no .deb found in release $tag."
      download "$url" "$tmp/agent-context-monitor.deb"
      say "Installing the .deb (you may be asked for your password)..."
      $SUDO apt-get install -y "$tmp/agent-context-monitor.deb"
      ;;
    rpm)
      url=$(asset ".x86_64.rpm")
      [ -n "$url" ] || fail "no .rpm found in release $tag."
      download "$url" "$tmp/agent-context-monitor.rpm"
      say "Installing the .rpm (you may be asked for your password)..."
      if has dnf; then
        $SUDO dnf install -y "$tmp/agent-context-monitor.rpm"
      else
        $SUDO zypper --non-interactive install --allow-unsigned-rpm "$tmp/agent-context-monitor.rpm"
      fi
      ;;
    appimage)
      url=$(asset "_amd64.AppImage")
      [ -n "$url" ] || fail "no AppImage found in release $tag."
      bin_dir="${ACM_BIN_DIR:-$HOME/.local/bin}"
      mkdir -p "$bin_dir"
      download "$url" "$tmp/agent-context-monitor.AppImage"
      chmod +x "$tmp/agent-context-monitor.AppImage"
      mv "$tmp/agent-context-monitor.AppImage" "$bin_dir/agent-context-monitor"
      say ""
      say "$APP_NAME $tag is installed at $bin_dir/agent-context-monitor."
      case ":$PATH:" in
        *":$bin_dir:"*) say "Start it by running: agent-context-monitor" ;;
        *) say "Start it by running: $bin_dir/agent-context-monitor" ;;
      esac
      return
      ;;
    *)
      fail "ACM_FORMAT must be deb, rpm, or appimage."
      ;;
  esac

  say ""
  say "$APP_NAME $tag is installed. Open it from your apps menu, or run: agent-context-monitor"
}

case "$(uname -s)" in
  Darwin) install_mac ;;
  Linux) install_linux ;;
  *) fail "this script is for macOS and Linux. On Windows, run this in PowerShell:
  irm https://raw.githubusercontent.com/$REPO/main/install.ps1 | iex" ;;
esac
