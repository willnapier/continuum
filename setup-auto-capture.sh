#!/usr/bin/env bash
# Continuum Auto-Capture Setup
# Sets up automatic conversation indexing for all assistants

set -e

CONTINUUM_DIR="$HOME/Assistants/projects/continuum"
BIN_DIR="$HOME/.local/bin"

echo "🔧 Continuum Auto-Capture Setup"
echo "================================"
echo

# Build if needed
if [ ! -f "$CONTINUUM_DIR/target/release/continuum" ]; then
    echo "📦 Building Continuum..."
    cd "$CONTINUUM_DIR"
    cargo build --release
    echo
fi

# Create bin directory if needed
mkdir -p "$BIN_DIR"

# Install binaries
echo "📥 Installing binaries to $BIN_DIR..."
ln -sf "$CONTINUUM_DIR/target/release/continuum" "$BIN_DIR/continuum"
ln -sf "$CONTINUUM_DIR/target/release/continuum-claude" "$BIN_DIR/continuum-claude"
echo "   ✓ continuum"
echo "   ✓ continuum-claude"
echo

# Detect shell
SHELL_CONFIG=""
if [ -n "$BASH_VERSION" ]; then
    SHELL_CONFIG="$HOME/.bashrc"
elif [ -n "$ZSH_VERSION" ]; then
    SHELL_CONFIG="$HOME/.zshrc"
elif command -v nu &> /dev/null; then
    SHELL_CONFIG="$HOME/.config/nushell/config.nu"
fi

# Set up Claude Code wrapper alias
if [ -n "$SHELL_CONFIG" ]; then
    echo "🔗 Setting up Claude Code auto-capture..."

    if [ "$SHELL_CONFIG" = "$HOME/.config/nushell/config.nu" ]; then
        ALIAS_LINE="alias claude = continuum-claude"
    else
        ALIAS_LINE="alias claude='continuum-claude'"
    fi

    if ! grep -q "continuum-claude" "$SHELL_CONFIG" 2>/dev/null; then
        echo >> "$SHELL_CONFIG"
        echo "# Continuum auto-capture for Claude Code" >> "$SHELL_CONFIG"
        echo "$ALIAS_LINE" >> "$SHELL_CONFIG"
        echo "   ✓ Added alias to $SHELL_CONFIG"
    else
        echo "   ✓ Alias already exists in $SHELL_CONFIG"
    fi
else
    echo "⚠️  Could not detect shell config. Add this manually:"
    echo "   alias claude='continuum-claude'"
fi
echo

# Set up systemd user service for auto-import
echo "⚙️  Setting up auto-import service..."

SYSTEMD_DIR="$HOME/.config/systemd/user"
mkdir -p "$SYSTEMD_DIR"

# Create the auto-import script
cat > "$BIN_DIR/continuum-auto-import" <<'EOF'
#!/usr/bin/env bash
# Auto-import sessions from Codex and Goose

CONTINUUM="$HOME/.local/bin/continuum"
LOG_FILE="$HOME/.local/share/continuum/auto-import.log"

mkdir -p "$(dirname "$LOG_FILE")"

log() {
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] $*" >> "$LOG_FILE"
}

# Import latest Codex session if exists
if [ -d "$HOME/.codex/sessions" ]; then
    if $CONTINUUM import --assistant codex >> "$LOG_FILE" 2>&1; then
        log "✓ Imported Codex session"
    else
        log "⚠ No new Codex session or error"
    fi
fi

# Import latest Goose session if exists
if [ -f "$HOME/.local/share/goose/sessions/sessions.db" ]; then
    if $CONTINUUM import --assistant goose >> "$LOG_FILE" 2>&1; then
        log "✓ Imported Goose session"
    else
        log "⚠ No new Goose session or error"
    fi
fi
EOF

chmod +x "$BIN_DIR/continuum-auto-import"
echo "   ✓ Created auto-import script"

# Create systemd timer (runs every 5 minutes)
cat > "$SYSTEMD_DIR/continuum-auto-import.timer" <<EOF
[Unit]
Description=Continuum Auto-Import Timer
Documentation=https://github.com/user/continuum

[Timer]
OnBootSec=1min
OnUnitActiveSec=5min
Persistent=true

[Install]
WantedBy=timers.target
EOF

# Create systemd service
cat > "$SYSTEMD_DIR/continuum-auto-import.service" <<EOF
[Unit]
Description=Continuum Auto-Import Service
Documentation=https://github.com/user/continuum

[Service]
Type=oneshot
ExecStart=$BIN_DIR/continuum-auto-import
EOF

echo "   ✓ Created systemd timer (runs every 5 minutes)"

# Enable and start the timer
if command -v systemctl &> /dev/null; then
    systemctl --user daemon-reload
    systemctl --user enable continuum-auto-import.timer
    systemctl --user start continuum-auto-import.timer
    echo "   ✓ Enabled and started auto-import timer"
else
    echo "   ⚠️  systemctl not found - timer created but not enabled"
fi
echo

echo "✅ Setup Complete!"
echo
echo "What happens now:"
echo "  • Claude Code: Use 'claude' command - auto-captures to database"
echo "  • Codex: Auto-imports every 5 minutes in background"
echo "  • Goose: Auto-imports every 5 minutes in background"
echo
echo "To verify it's working:"
echo "  • Chat with any assistant"
echo "  • Wait a few minutes (or restart your shell for Claude alias)"
echo "  • Run: continuum search --query 'test'"
echo
echo "To check auto-import logs:"
echo "  • tail -f ~/.local/share/continuum/auto-import.log"
echo
echo "To check timer status:"
echo "  • systemctl --user status continuum-auto-import.timer"
echo
echo "⚠️  IMPORTANT: Restart your shell or run:"
echo "  source $SHELL_CONFIG"
echo
EOF

chmod +x "$CONTINUUM_DIR/setup-auto-capture.sh"
