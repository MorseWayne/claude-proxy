# claude-proxy

A Claude-compatible proxy that routes requests to OpenAI, Anthropic, **GitHub Copilot**, **ChatGPT**, or any OpenAI-compatible upstream provider.

Single native binary, zero runtime dependencies.

## Install

Linux / macOS:

```bash
curl -fsSL https://github.com/MorseWayne/claude-proxy/releases/latest/download/install.sh | bash
```

Windows PowerShell:

```powershell
irm https://github.com/MorseWayne/claude-proxy/releases/latest/download/install.ps1 | iex
```

Or download from [GitHub Releases](https://github.com/MorseWayne/claude-proxy/releases).

## Quick Start

```bash
# Add a provider (auto-fetches available models for default selection)
claude-proxy provider add openai

# Add GitHub Copilot (auto OAuth authentication)
claude-proxy provider add copilot

# Add ChatGPT (OAuth with a ChatGPT Pro/Plus account)
claude-proxy provider add chatgpt

# Start the server
claude-proxy server start

# Point Claude Code at the proxy
export ANTHROPIC_BASE_URL=http://127.0.0.1:8082
export ANTHROPIC_API_KEY=freecc
```

## CLI Commands

### Provider Management

```bash
claude-proxy provider list              # List configured providers
claude-proxy provider current           # Show current default model
claude-proxy provider add [id]          # Add a provider (interactive if ID omitted)
claude-proxy provider edit <id>         # Edit provider config
claude-proxy provider delete <id>       # Delete a provider
claude-proxy provider switch <id>       # Set default model
claude-proxy provider test <id>         # Test API key
claude-proxy provider speedtest <id>    # Latency test
claude-proxy provider fetch-models <id> # Fetch and cache model list
```

### Configuration

```bash
claude-proxy config show                # Show config (keys masked)
claude-proxy config edit                # Open config in $EDITOR
claude-proxy config validate            # Validate config
claude-proxy config path                # Print config file path
claude-proxy config export [path]       # Export config (stdout if no path)
claude-proxy config import <path>       # Import config from file
```

### Server

```bash
claude-proxy server start               # Start proxy server (foreground)
claude-proxy server start --daemon      # Start as daemon (Unix only)
claude-proxy server stop                # Stop daemon (Unix only)
claude-proxy server restart             # Graceful config reload via SIGUSR1 (Unix only)
claude-proxy server status              # Check if daemon is running
```

### Shell Completions

```bash
claude-proxy completions bash           # Generate bash completions
claude-proxy completions zsh            # Generate zsh completions
claude-proxy completions fish           # Generate fish completions
```

Add to shell (example for bash):

```bash
eval "$(claude-proxy completions bash)"
```

### TUI Configuration

```bash
claude-proxy tui                        # Launch interactive terminal UI config interface
```

Keyboard-navigable TUI for configuration management, provider management, and model list browsing.

![TUI Model Selection](images/tui-model-selection.png)

## Configuration

Config file: `~/.config/claude-proxy/config.toml`

```toml
[providers.openai]
api_key = "sk-..."
base_url = "https://api.openai.com/v1"
proxy = ""                              # Optional HTTP proxy

# GitHub Copilot provider (OAuth auto-authentication, no api_key needed)
[providers.copilot]
base_url = "https://api.githubcopilot.com"

[providers.copilot.copilot]
oauth_app = "vscode"                    # OAuth app: "vscode" or "opencode"
small_model = "gpt-5-mini"             # Warmup fallback model
max_thinking_tokens = 16000             # Maximum thinking tokens
enable_warmup = true                    # Enable warmup detection (route tool-less requests to small model)
enable_tool_result_merge = true         # Enable tool_result merging (reduce premium billing)
enable_compact_detection = true         # Enable compact/auto-continue detection
enable_agent_marking = true             # Enable sub-agent traffic marking

# ChatGPT provider (OAuth auto-authentication, no api_key needed)
[providers.chatgpt]
base_url = "https://chatgpt.com/backend-api/codex"

[model]
default = "openai/gpt-4.1"
reasoning = "openai/o4-mini"                    # Optional, synced as ANTHROPIC_REASONING_MODEL
opus = "anthropic/claude-opus-4-20250514"      # Optional model aliases
sonnet = "anthropic/claude-sonnet-4-20250514"
haiku = "anthropic/claude-haiku-4-5-20251001"

[server]
host = "127.0.0.1"
port = 8082
auth_token = "freecc"                   # API key required from clients

[admin]
auth_token = ""                         # Admin API token (empty = fallback to server.auth_token)

[limits]
rate_limit = 40                         # Max requests per window
rate_window = 60                        # Window in seconds
max_concurrency = 5                     # Max concurrent requests
provider_max_concurrency = 4            # Max concurrent upstream requests per provider

[http]
read_timeout = 300                      # Upstream read timeout (seconds)
write_timeout = 60                      # Upstream write timeout (seconds)
connect_timeout = 60                    # Upstream connect timeout (seconds)

[log]
level = "info"                          # Log level (trace/debug/info/warn/error)
file = ""                               # Optional, log file path (defaults to config_dir/claude-proxy.log)
with_stdout = true                      # Also emit to stderr (foreground server and CLI)
raw_api_payloads = false                # Log raw request payloads
raw_sse_events = false                  # Log raw SSE events
```

### Model Routing

The `default` model field uses `provider_id/upstream_model` format. For example, `openai/gpt-4.1` routes to the `openai` provider and sends `gpt-4.1` as the model name upstream.

Claude model names (e.g., `claude-opus-4-20250514`) are automatically resolved through the `[model]` aliases. If no alias matches, the request model is used as-is with the default provider.

## HTTP API

### Proxy Endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/health` | Health check |
| `POST` | `/v1/messages` | Anthropic Messages API proxy |
| `GET` | `/v1/models` | List available models |

### Admin Endpoints

All admin endpoints require `Authorization: Bearer <admin_token>`. Falls back to `server.auth_token` if admin_token is not set.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/admin/config` | Get current config (keys masked) |
| `PUT` | `/admin/config` | Update config (`{"config": "<toml>"}`) |
| `POST` | `/admin/restart` | Reload config from disk |
| `GET` | `/admin/metrics` | Get request metrics (including all-time history) |

`GET /admin/metrics` response format:

```json
{
  "requests_total": 42,
  "errors_total": 1,
  "avg_latency_ms": 320,
  "models": {
    "openai/gpt-4.1": {
      "requests": 30,
      "input_tokens": 15000,
      "output_tokens": 8000,
      "cache_creation_input_tokens": 0,
      "cache_read_input_tokens": 2000
    }
  },
  "stored": {
    "requests_total": 1500,
    "errors_total": 12,
    "avg_latency_ms": 305,
    "models": { ... }
  }
}
```

- Top-level fields: current process session statistics
- `stored` field: all-time cumulative data persisted in SQLite (survives restarts)
- Dashboard automatically merges both layers to show totals

## Features

- **Multi-Provider**: OpenAI, Anthropic, GitHub Copilot, and any OpenAI-compatible API
- **Copilot Integration**: Full GitHub OAuth auth, VS Code impersonation, premium request optimization
- **Auto Model Discovery**: Fetches available models when adding a provider, interactive default model selection
- **TUI Config Interface**: Built-in terminal UI with keyboard navigation for config and provider management
- **Rate Limiting**: Per-API-key rate limiting using token bucket algorithm
- **Concurrency Control**: Semaphore-based concurrency limiting with timeout
- **Config Hot-Reload**: Config file watcher + SIGUSR1 signal for live reload
- **Daemon Mode**: Background process with PID file management (Unix)
- **Model Cache Warmup**: Pre-fetches model lists from all providers on startup
- **Token Usage Metrics**: Per-model input/output/cache token usage tracking with real-time session data and persistent all-time history
- **Persistent Storage**: Usage data automatically stored in SQLite (`~/.config/claude-proxy/metrics.db`), survives restarts
- **TUI Dashboard**: Terminal dashboard showing live request count, error rate, latency, and per-model token usage
- **Config Migration**: Auto-migration from legacy `.env` to TOML config
- **Graceful Shutdown**: Handles SIGINT and SIGTERM for clean exit

## Screenshots

### TUI Dashboard
![TUI Dashboard](images/metrics-dashboard.png)

### Using with Claude Code
![Claude Code Integration](images/claude-code-usage.png)

## Build from Source

```bash
cargo build --release
# binary at target/release/claude-proxy
```

Build the Linux musl statically linked version:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
# binary at target/x86_64-unknown-linux-musl/release/claude-proxy
```

## License

MIT
