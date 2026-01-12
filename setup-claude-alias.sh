#!/usr/bin/env bash
# Setup script for continuum-claude wrapper
# Works on both Linux and macOS

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_BIN="$HOME/.local/bin"

echo "Setting up continuum-claude wrapper..."

# Create .local/bin if it doesn't exist
mkdir -p "$TARGET_BIN"

# Build the binary
echo "Building continuum-claude..."
cargo build --release --bin continuum-claude

# Copy or symlink the binary
if [ -L "$TARGET_BIN/continuum-claude" ]; then
    echo "Removing existing symlink..."
    rm "$TARGET_BIN/continuum-claude"
fi

echo "Creating symlink..."
ln -sf "$SCRIPT_DIR/target/release/continuum-claude" "$TARGET_BIN/continuum-claude"

# Check if PATH includes ~/.local/bin
if [[ ":$PATH:" != *":$HOME/.local/bin:"* ]]; then
    echo ""
    echo "⚠️  Warning: ~/.local/bin is not in your PATH"
    echo "Add this to your shell config:"
    echo '  export PATH="$HOME/.local/bin:$PATH"'
fi

# Setup bash alias if using bash
if [ -f "$HOME/.bashrc" ]; then
    if ! grep -q "alias claude='continuum-claude'" "$HOME/.bashrc"; then
        echo ""
        echo "Adding bash alias to ~/.bashrc"
        echo "# Continuum auto-capture for Claude Code" >> "$HOME/.bashrc"
        echo "alias claude='continuum-claude'" >> "$HOME/.bashrc"
        echo "✓ Bash alias added"
    else
        echo "✓ Bash alias already exists"
    fi
fi

# Setup zsh alias if using zsh
if [ -f "$HOME/.zshrc" ]; then
    if ! grep -q "alias claude='continuum-claude'" "$HOME/.zshrc"; then
        echo ""
        echo "Adding zsh alias to ~/.zshrc"
        echo "# Continuum auto-capture for Claude Code" >> "$HOME/.zshrc"
        echo "alias claude='continuum-claude'" >> "$HOME/.zshrc"
        echo "✓ Zsh alias added"
    else
        echo "✓ Zsh alias already exists"
    fi
fi

# Check Nushell setup
if [ -f "$HOME/.config/nushell/config.nu" ]; then
    if grep -q "def claude \[...args\]" "$HOME/.config/nushell/config.nu"; then
        echo "✓ Nushell wrapper function exists"
    else
        echo ""
        echo "⚠️  Nushell config.nu exists but no claude wrapper found"
        echo "The claude wrapper function should already be in your config.nu"
    fi
fi

echo ""
echo "✅ Setup complete!"
echo ""
echo "The 'claude' command will now:"
echo "  • Use continuum-claude wrapper automatically"
echo "  • Work in interactive mode (full Claude Code experience)"
echo "  • Work in --print mode (with automatic logging)"
echo ""
echo "Test it with:"
echo "  claude --version"
echo ""
echo "Note: Restart your shell or run 'source ~/.bashrc' to activate"
