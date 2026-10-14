#!/usr/bin/env sh
# Installs the Impactor MCP server (impactor-mcp) and registers it with Claude Code /
# Claude Desktop when present. Any other MCP client can point at the installed binary.
# Works alongside the stock Impactor app - nothing about the app is modified.
#
# Run straight from the network (nothing to clone):
#   curl -fsSL https://raw.githubusercontent.com/YeetDisDude/impactor-mcp/main/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --auto-refresh
#
# Re-running it updates an existing install to the latest release and keeps the
# auto-refresh setting unless --auto-refresh / --no-auto-refresh is given.
#
# Options: --no-claude-code --no-desktop --auto-refresh --no-auto-refresh --from-source --uninstall
set -eu

REPO="YeetDisDude/impactor-mcp"
SERVER_NAME="impactor"
INSTALL_DIR="${IMPACTOR_MCP_INSTALL_DIR:-$HOME/.local/bin}"
BIN_PATH="$INSTALL_DIR/impactor-mcp"

# A local checkout has Cargo.toml next to this script; when piped into `sh`, $0 is the shell.
LOCAL_ROOT=""
case "$0" in
    */install.sh|install.sh)
        dir="$(cd "$(dirname "$0")" && pwd)"
        [ -f "$dir/Cargo.toml" ] && LOCAL_ROOT="$dir"
        ;;
esac

OS="$(uname -s)"
case "$OS" in
    Darwin) PLATFORM=macos; DESKTOP_CONFIG="$HOME/Library/Application Support/Claude/claude_desktop_config.json" ;;
    Linux) PLATFORM=linux; DESKTOP_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/Claude/claude_desktop_config.json" ;;
    *) echo "Unsupported OS: $OS (use install.ps1 on Windows)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    arm64|aarch64) ARCH=aarch64 ;;
    *) ARCH=x86_64 ;;
esac

# AUTO_REFRESH stays empty unless given, so an update can keep the current setting.
NO_CLAUDE_CODE=0; NO_DESKTOP=0; AUTO_REFRESH=""; FROM_SOURCE=0; UNINSTALL=0
for arg in "$@"; do
    case "$arg" in
        --no-claude-code) NO_CLAUDE_CODE=1 ;;
        --no-desktop) NO_DESKTOP=1 ;;
        --auto-refresh) AUTO_REFRESH=1 ;;
        --no-auto-refresh) AUTO_REFRESH=0 ;;
        --from-source) FROM_SOURCE=1 ;;
        --uninstall) UNINSTALL=1 ;;
        -h|--help) echo "Options: --no-claude-code --no-desktop --auto-refresh --no-auto-refresh --from-source --uninstall"; exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done

step() { printf '\033[36m==> %s\033[0m\n' "$1"; }
warn() { printf '\033[33mwarning: %s\033[0m\n' "$1" >&2; }

# Edits claude_desktop_config.json with python3 (preinstalled on macOS and most
# Linux distros). $1 = "add" | "remove", $2 = server name (default $SERVER_NAME)
desktop_config() {
    if ! command -v python3 >/dev/null 2>&1; then
        warn "python3 not found; add this to $DESKTOP_CONFIG under \"mcpServers\":"
        echo "  \"$SERVER_NAME\": { \"command\": \"$BIN_PATH\", \"args\": [] }"
        return 0
    fi
    python3 - "$1" "$DESKTOP_CONFIG" "${2:-$SERVER_NAME}" "$BIN_PATH" "$AUTO_REFRESH" <<'PY'
import json, os, shutil, sys
action, path, name, binary, auto = sys.argv[1:6]
config = {}
if os.path.exists(path):
    shutil.copyfile(path, path + ".bak")
    with open(path, encoding="utf-8") as f:
        text = f.read().strip()
        config = json.loads(text) if text else {}
servers = config.setdefault("mcpServers", {})
if action == "add":
    entry = {"command": binary, "args": []}
    if auto == "1":
        entry["env"] = {"IMPACTOR_MCP_AUTO_REFRESH": "1"}
    servers[name] = entry
else:
    servers.pop(name, None)
os.makedirs(os.path.dirname(path), exist_ok=True)
with open(path, "w", encoding="utf-8") as f:
    json.dump(config, f, indent=2)
PY
}

# Whether the current registration has auto-refresh on (Claude Desktop config or
# Claude Code). Prints 1 or 0.
detect_auto_refresh() {
    if [ -f "$DESKTOP_CONFIG" ] && command -v python3 >/dev/null 2>&1; then
        found="$(python3 - "$DESKTOP_CONFIG" "$SERVER_NAME" <<'PY' 2>/dev/null || true
import json, sys
try:
    with open(sys.argv[1], encoding="utf-8") as f:
        entry = json.load(f).get("mcpServers", {}).get(sys.argv[2])
except Exception:
    entry = None
if entry is not None:
    value = str((entry.get("env") or {}).get("IMPACTOR_MCP_AUTO_REFRESH", "")).lower()
    print(1 if value in ("1", "true") else 0)
PY
)"
        if [ -n "$found" ]; then echo "$found"; return; fi
    fi
    if command -v claude >/dev/null 2>&1 \
        && claude mcp get "$SERVER_NAME" 2>/dev/null | grep -qiE 'IMPACTOR_MCP_AUTO_REFRESH=(1|true)'; then
        echo 1; return
    fi
    echo 0
}

# Moves a new binary into place with a rename, which works even while an MCP
# client is running the old one (it keeps the old file until it restarts).
put_binary() { # src
    cp -f "$1" "$BIN_PATH.new"
    chmod +x "$BIN_PATH.new"
    mv -f "$BIN_PATH.new" "$BIN_PATH"
}

download() { # url dest
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL "$1" -o "$2"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "$2" "$1"
    else
        return 1
    fi
}

install_prebuilt() {
    url="https://github.com/$REPO/releases/latest/download/impactor-mcp-$PLATFORM-$ARCH"
    step "Downloading prebuilt binary ($url)"
    tmp="$(mktemp)"
    if ! download "$url" "$tmp" || [ ! -s "$tmp" ]; then
        rm -f "$tmp"
        warn "No prebuilt binary available; building from source instead."
        return 1
    fi
    put_binary "$tmp"
    rm -f "$tmp"
}

install_from_source() {
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    if ! command -v cargo >/dev/null 2>&1; then
        step "Rust not found; installing rustup (https://rustup.rs)"
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
        PATH="$HOME/.cargo/bin:$PATH"
    fi
    step "Building impactor-mcp (release). The first build downloads dependencies and takes a few minutes..."
    if [ -n "$LOCAL_ROOT" ]; then
        (cd "$LOCAL_ROOT" && cargo build --release --locked)
        put_binary "$LOCAL_ROOT/target/release/impactor-mcp"
    else
        root="$(mktemp -d)"
        cargo install --locked --git "https://github.com/$REPO" impactor-mcp --root "$root"
        put_binary "$root/bin/impactor-mcp"
        rm -rf "$root"
    fi
}

# Earlier releases were named `plume-impactor` / `plumemcp`; clean those up.
remove_legacy() {
    if command -v claude >/dev/null 2>&1; then
        claude mcp remove --scope user plume-impactor >/dev/null 2>&1 || true
    fi
    if [ -f "$DESKTOP_CONFIG" ] && grep -q '"plume-impactor"' "$DESKTOP_CONFIG" 2>/dev/null; then
        desktop_config remove plume-impactor
    fi
    rm -f "$INSTALL_DIR/plumemcp"
}

# --- Uninstall ---------------------------------------------------------------
if [ "$UNINSTALL" = 1 ]; then
    remove_legacy
    if command -v claude >/dev/null 2>&1; then
        claude mcp remove --scope user "$SERVER_NAME" >/dev/null 2>&1 || true
    fi
    if [ -f "$DESKTOP_CONFIG" ]; then desktop_config remove; fi
    rm -f "$BIN_PATH"
    step "Uninstalled. (Impactor accounts in its config directory were left untouched.)"
    exit 0
fi

# --- 1. Get the binary -------------------------------------------------------
UPDATING=0
[ -f "$BIN_PATH" ] && UPDATING=1
if [ -z "$AUTO_REFRESH" ]; then
    AUTO_REFRESH="$(detect_auto_refresh)"
    [ "$UPDATING" = 1 ] && [ "$AUTO_REFRESH" = 1 ] && step "Keeping auto-refresh on (pass --no-auto-refresh to turn it off)"
fi
mkdir -p "$INSTALL_DIR"
rm -f "$BIN_PATH.new"
# Prefer the prebuilt binary (no compiling); build only when asked or when none exists.
if [ "$FROM_SOURCE" = 1 ] || ! install_prebuilt; then
    install_from_source
fi
if [ "$UPDATING" = 1 ]; then step "Updated $BIN_PATH"; else step "Installed $BIN_PATH"; fi
remove_legacy

# --- 2. Register -------------------------------------------------------------
if [ "$NO_CLAUDE_CODE" = 0 ]; then
    if command -v claude >/dev/null 2>&1; then
        step "Registering with Claude Code (user scope)"
        claude mcp remove --scope user "$SERVER_NAME" >/dev/null 2>&1 || true
        if [ "$AUTO_REFRESH" = 1 ]; then
            claude mcp add --scope user -e IMPACTOR_MCP_AUTO_REFRESH=1 "$SERVER_NAME" -- "$BIN_PATH"
        else
            claude mcp add --scope user "$SERVER_NAME" -- "$BIN_PATH"
        fi
    else
        warn "Claude Code CLI not found; skipped. Later run: claude mcp add --scope user $SERVER_NAME -- \"$BIN_PATH\""
    fi
fi

if [ "$NO_DESKTOP" = 0 ]; then
    step "Registering with Claude Desktop ($DESKTOP_CONFIG)"
    desktop_config add
    echo "    Restart Claude Desktop to load the server."
fi

if [ "$UPDATING" = 1 ]; then
    echo "    Running Claude Code sessions keep the old version until restarted."
fi

# --- 3. Prerequisite check ---------------------------------------------------
if [ "$PLATFORM" = linux ] && ! command -v usbmuxd >/dev/null 2>&1; then
    warn "usbmuxd not found. Install it (e.g. 'sudo apt install usbmuxd') so the server can talk to iPhones/iPads."
fi

step "Done. Other MCP clients: use command \"$BIN_PATH\" (stdio). Try asking your agent: \"list my connected iOS devices\"."
