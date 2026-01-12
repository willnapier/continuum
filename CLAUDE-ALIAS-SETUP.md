# Claude Alias Setup for Continuum

This document explains how the `claude` alias/wrapper works across Linux and macOS.

## How It Works

When you type `claude`, the system uses `continuum-claude` wrapper which:

1. **Interactive mode** (default): Execs to the real Claude Code binary
   - Preserves full TTY/terminal functionality
   - No logging (yet) - just transparent passthrough

2. **Print mode** (`--print` flag): Wraps and logs conversation
   - Captures output in `stream-json` format
   - Writes to `~/continuum-logs/claude-code/`

## Setup on Linux (Already Done)

Your Linux system is already set up:
- ✅ Binary: `~/.local/bin/continuum-claude`
- ✅ Bash alias: `~/.bashrc:153`
- ✅ Nushell function: `~/.config/nushell/config.nu:3682`

## Setup on macOS

To set up on your Mac:

```bash
cd ~/Assistants/projects/continuum
./setup-claude-alias.sh
```

This will:
1. Build `continuum-claude` binary
2. Symlink to `~/.local/bin/continuum-claude`
3. Add alias to `~/.bashrc` and/or `~/.zshrc`
4. Check your Nushell config

### Manual Setup (if needed)

If the script doesn't work, do this manually:

```bash
# Build and link
cd ~/Assistants/projects/continuum
cargo build --release --bin continuum-claude
ln -sf ~/Assistants/projects/continuum/target/release/continuum-claude ~/.local/bin/

# Add to shell config
echo "alias claude='continuum-claude'" >> ~/.zshrc  # or ~/.bashrc
```

For Nushell, add this function to `~/.config/nushell/config.nu`:

```nushell
def claude [...args] {
    let continuum_claude = ($env.HOME | path join ".local/bin/continuum-claude")

    if ($continuum_claude | path exists) {
        ^$continuum_claude ...$args
    } else {
        # Fallback to real claude
        ^/usr/local/bin/claude ...$args  # or /opt/homebrew/bin/claude
    }
}
```

## Troubleshooting

### "claude: command not found"

Check if `~/.local/bin` is in your PATH:

```bash
echo $PATH | grep .local/bin
```

If not, add to your shell config:

```bash
# For bash/zsh
export PATH="$HOME/.local/bin:$PATH"
```

```nushell
# For Nushell (in env.nu)
$env.PATH = ($env.PATH | split row (char esep) | prepend $"($env.HOME)/.local/bin")
```

### "Could not find real claude binary"

The wrapper looks for Claude in these locations:
- Linux: `/usr/bin/claude`
- macOS Intel: `/usr/local/bin/claude`
- macOS Apple Silicon: `/opt/homebrew/bin/claude`

Check where Claude is installed:

```bash
which -a claude
```

If it's in a different location, you can:
1. Create a symlink to one of the expected locations
2. Modify the fallback paths in `continuum-claude/src/main.rs:239-243`

### "continuum-claude only supports --print mode"

This error means you're running an old version. Rebuild:

```bash
cd ~/Assistants/projects/continuum
cargo build --release --bin continuum-claude
```

### Testing

Test the setup:

```bash
# Should show version
claude --version

# Should open interactive Claude
claude

# Should log to continuum
echo "test" | claude --print
```

## Where Claude Lives

### Linux
- npm global: `/usr/bin/claude` → symlink to npm installation
- Actual binary: `/usr/lib/node_modules/@anthropic-ai/claude-code/cli.js`

### macOS Intel
- npm global: `/usr/local/bin/claude`
- Actual binary: `/usr/local/lib/node_modules/@anthropic-ai/claude-code/cli.js`

### macOS Apple Silicon
- Homebrew: `/opt/homebrew/bin/claude`
- npm global: `/opt/homebrew/bin/claude` or `/usr/local/bin/claude`

## Implementation Details

### Bash/Zsh Alias
```bash
alias claude='continuum-claude'
```

Simple and effective - just redirects all `claude` commands to the wrapper.

### Nushell Function
```nushell
def claude [...args] {
    ^$continuum_claude ...$args
}
```

More sophisticated - can add logic for environment variables, logging, etc.

### Rust Wrapper (`continuum-claude`)

The wrapper binary:
1. Checks if `--print` flag is present
2. If yes: wraps with logging (existing functionality)
3. If no: uses `exec()` to replace itself with real claude binary

The `exec()` approach is important because it:
- Preserves TTY/terminal control
- Maintains signal handling
- Zero overhead (process replacement, not spawning)

## Future Enhancements

Possible improvements:

1. **Interactive logging**: Capture interactive sessions too
   - Would need to hook into Claude Code's session system
   - Could use filesystem watcher on Claude's session storage

2. **Automatic mode detection**: Don't require `--print`
   - Detect if stdin is a TTY
   - Auto-add flags for logging

3. **Cross-platform path detection**: Better discovery of Claude installation
   - Check npm global prefix: `npm config get prefix`
   - Check for homebrew: `brew --prefix`

## See Also

- [QUICK-START.md](QUICK-START.md) - General Continuum setup
- [USAGE.md](USAGE.md) - How to use Continuum functions
- [continuum-claude source](continuum-claude/src/main.rs) - Wrapper implementation
