# claude-proxy

A Claude-compatible proxy that routes requests to OpenAI-compatible or Anthropic Messages-compatible upstream providers.

Single native binary, zero runtime dependencies.

## Install

```bash
curl -fsSL https://github.com/MorseWayne/claude-proxy/releases/latest/download/install.sh | bash
```

Or download from [GitHub Releases](https://github.com/MorseWayne/claude-proxy/releases).

## Quick Start

```bash
# Add a provider
claude-proxy provider add openai

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

## Configuration

Config file: `~/.config/claude-proxy/config.toml`

```toml
[providers.openai]
api_key = "sk-..."
base_url = "https://api.openai.com/v1"
proxy = ""                              # Optional HTTP proxy

[model]
default = "openai/gpt-4.1"
opus = "anthropic/claude-opus-4-20250514"      # Optional model aliases
sonnet = "anthropic/claude-sonnet-4-20250514"
haiku = "anthropic/claude-haiku-4-5-20251001"

[server]
host = "127.0.0.1"
port = 8082
auth_token = "freecc"                   # API key required from clients

[admin]
auth_token = ""                         # Admin API token (empty = disabled)

[limits]
rate_limit = 100                        # Max requests per window
rate_window = 60                        # Window in seconds
max_concurrency = 10                    # Max concurrent requests

[http]
read_timeout = 300                      # Upstream read timeout (seconds)
write_timeout = 60                      # Upstream write timeout (seconds)
connect_timeout = 10                    # Upstream connect timeout (seconds)

[log]
level = "info"                          # Log level (trace/debug/info/warn/error)
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

All admin endpoints require `Authorization: Bearer <admin_token>`.

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/admin/config` | Get current config (keys masked) |
| `PUT` | `/admin/config` | Update config (`{"config": "<toml>"}`) |
| `POST` | `/admin/restart` | Reload config from disk |
| `GET` | `/admin/metrics` | Get request metrics |

## Features

- **Rate Limiting**: Per-API-key rate limiting using token bucket algorithm
- **Concurrency Control**: Semaphore-based concurrency limiting with timeout
- **Config Hot-Reload**: Config file watcher + SIGUSR1 signal for live reload
- **Daemon Mode**: Background process with PID file management (Unix)
- **Model Cache Warms**: Pre-fetches model lists from all providers on startup
- **Graceful Shutdown**: Handles SIGINT and SIGTERM for clean exit

## Build from Source

```bash
cargo build --release
# binary at target/release/claude-proxy
```

## License

MIT
