use std::path::PathBuf;
use std::process;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use claude_proxy_config::settings::{ProviderConfig, ProviderType};
use colored::Colorize;

mod logging;
mod tui;

#[derive(Parser)]
#[command(
    name = "claude-proxy",
    version,
    about = "Claude-compatible proxy for OpenAI and Anthropic providers"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Provider management
    Provider {
        #[command(subcommand)]
        action: ProviderAction,
    },
    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Server management
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Generate shell completions
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Launch interactive TUI configuration interface
    Tui,
}

#[derive(Subcommand)]
enum ProviderAction {
    /// List all configured providers
    List,
    /// Show the current default model
    Current,
    /// Add a new provider
    Add {
        /// Optional provider ID or known provider type (e.g., "copilot")
        id: Option<String>,
    },
    /// Edit a provider's configuration
    Edit {
        /// Provider ID to edit
        id: String,
    },
    /// Delete a provider
    Delete {
        /// Provider ID to delete
        id: String,
    },
    /// Switch the default model to a provider
    Switch {
        /// Provider ID to switch to
        id: String,
    },
    /// Test a provider's API key
    Test {
        /// Provider ID to test
        id: String,
    },
    /// Speed test a provider's latency
    Speedtest {
        /// Provider ID to test
        id: String,
    },
    /// Fetch and cache a provider's model list
    FetchModels {
        /// Provider ID to fetch models for
        id: String,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration (keys masked)
    Show,
    /// Open config in $EDITOR
    Edit,
    /// Validate the configuration
    Validate,
    /// Print the config file path
    Path,
    /// Export config to a file
    Export {
        /// Output path (defaults to stdout)
        path: Option<PathBuf>,
    },
    /// Import config from a file
    Import {
        /// Input file path
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ServerAction {
    /// Start the proxy server
    Start {
        /// Run as a daemon (Unix only)
        #[arg(long)]
        daemon: bool,
    },
    /// Stop the daemon (Unix only)
    Stop,
    /// Graceful restart via SIGUSR1 (Unix only)
    Restart,
    /// Show server status
    Status,
}

fn main() {
    let cli = Cli::parse();

    // Handle daemon mode before tokio runtime starts (Unix only)
    #[cfg(unix)]
    if let Commands::Server {
        action: ServerAction::Start { daemon: true },
    } = &cli.command
    {
        daemonize_process();
    }

    // Create tokio runtime and run async main
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    rt.block_on(async_main(cli));
}

async fn async_main(cli: Cli) {
    // Initialize logging (early — before any work)
    let is_tui = matches!(cli.command, Commands::Tui);
    let settings = claude_proxy_config::Settings::config_file_path()
        .filter(|p| p.exists())
        .and_then(|p| claude_proxy_config::Settings::load(&p).ok());

    let log_config = settings.as_ref().map(|s| &s.log);
    if let Err(e) = logging::init_logging(log_config.unwrap_or(&Default::default()), is_tui) {
        eprintln!("Warning: failed to initialize logging: {e}");
    }

    match cli.command {
        Commands::Provider { action } => handle_provider(action).await,
        Commands::Config { action } => handle_config(action).await,
        Commands::Server { action } => handle_server(action).await,
        Commands::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "claude-proxy", &mut std::io::stdout());
        }
        Commands::Tui => {
            if let Err(e) = tui::run() {
                eprintln!("{} TUI error: {e}", "Error:".red().bold());
                process::exit(1);
            }
        }
    }
}

#[cfg(unix)]
fn daemonize_process() {
    use daemonize::{Daemonize, Outcome};

    let config_dir = claude_proxy_config::Settings::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let pid_file = config_dir.join("claude-proxy.pid");
    let log_file = config_dir.join("claude-proxy.log");

    // Create config dir if it doesn't exist
    let _ = std::fs::create_dir_all(&config_dir);

    let daemon = Daemonize::new()
        .pid_file(&pid_file)
        .chown_pid_file(true)
        .stdout(std::fs::File::create(&log_file).unwrap_or_else(|e| {
            eprintln!("Failed to create log file {}: {e}", log_file.display());
            std::process::exit(1);
        }))
        .stderr(std::fs::File::create(&log_file).unwrap_or_else(|e| {
            eprintln!("Failed to create log file {}: {e}", log_file.display());
            std::process::exit(1);
        }));

    match daemon.execute() {
        Outcome::Parent(Ok(_)) => {
            // Parent exits; child continues with tokio runtime
            // PID is written to pid_file by daemonize
            if let Ok(content) = std::fs::read_to_string(&pid_file) {
                println!("Daemon started with PID {}", content.trim());
            } else {
                println!("Daemon started");
            }
            std::process::exit(0);
        }
        Outcome::Parent(Err(e)) => {
            eprintln!("Failed to start daemon: {e}");
            std::process::exit(1);
        }
        Outcome::Child(Ok(_)) => {
            // Child continues — tokio runtime will start after this function returns
        }
        Outcome::Child(Err(e)) => {
            eprintln!("Daemon child error: {e}");
            std::process::exit(1);
        }
    }
}

async fn handle_provider(action: ProviderAction) {
    match action {
        ProviderAction::List => {
            let settings = load_settings_or_exit();
            println!("{}", "Configured providers:".bold());
            println!();
            if settings.providers.is_empty() {
                println!("  {}", "No providers configured.".yellow());
            } else {
                for (id, provider) in &settings.providers {
                    println!(
                        "  {} {}",
                        id.green().bold(),
                        format!("({})", provider.base_url).dimmed()
                    );
                    if provider.uses_oauth(id) {
                        println!("    Auth: OAuth (automatic)");
                    } else {
                        let masked_key = mask_key(&provider.api_key);
                        println!("    API Key: {masked_key}");
                    }
                    if !provider.proxy.is_empty() {
                        println!("    Proxy: {}", provider.proxy);
                    }
                }
            }
            println!();
            println!(
                "  {} {}",
                "Default model:".bold(),
                settings.model.default.cyan()
            );
        }
        ProviderAction::Current => {
            let settings = load_settings_or_exit();
            println!("{}", settings.model.default.cyan());
        }
        ProviderAction::Add { id } => {
            let mut settings = match claude_proxy_config::Settings::config_file_path() {
                Some(path) if path.exists() => claude_proxy_config::Settings::load(&path)
                    .unwrap_or_else(|e| {
                        eprintln!("{} Failed to load config: {e}", "Error:".red().bold());
                        process::exit(1);
                    }),
                _ => claude_proxy_config::Settings::default(),
            };

            let (provider_id, provider_type) = match id {
                Some(provider_id) => {
                    let inferred_type = ProviderType::parse(&provider_id);
                    let provider_type = if is_custom_provider_type(&inferred_type) {
                        prompt_provider_type()
                    } else {
                        println!(
                            "Provider type: {} (inferred from \"{}\")",
                            inferred_type.display_name(),
                            provider_id
                        );
                        inferred_type
                    };
                    (provider_id, provider_type)
                }
                None => {
                    let provider_type = prompt_provider_type();
                    let provider_id = if is_custom_provider_type(&provider_type) {
                        dialoguer::Input::new()
                            .with_prompt("Provider ID")
                            .interact_text()
                            .unwrap()
                    } else {
                        let id = provider_type.as_str().to_string();
                        println!("Provider ID: {}", id.green());
                        id
                    };
                    (provider_id, provider_type)
                }
            };

            let provider_type = match provider_type {
                ProviderType::Custom(_) => ProviderType::Custom(provider_id.clone()),
                ProviderType::CustomAnthropic(_) => {
                    ProviderType::CustomAnthropic(provider_id.clone())
                }
                other => other,
            };

            let api_key: String = if provider_type.needs_api_key() {
                dialoguer::Password::new()
                    .with_prompt("API Key")
                    .interact()
                    .unwrap()
            } else {
                String::new()
            };

            let base_url: String = dialoguer::Input::new()
                .with_prompt("Base URL")
                .default(provider_type.default_base_url().to_string())
                .interact_text()
                .unwrap();

            let proxy: String = dialoguer::Input::new()
                .with_prompt("Proxy (optional)")
                .default("".to_string())
                .interact_text()
                .unwrap();

            let copilot = if provider_type == ProviderType::Copilot {
                Some(claude_proxy_config::settings::CopilotProviderConfig::default())
            } else {
                None
            };

            let replaced = settings.providers.contains_key(&provider_id);
            settings.providers.insert(
                provider_id.clone(),
                ProviderConfig {
                    api_key,
                    base_url: base_url.clone(),
                    proxy,
                    provider_type: Some(provider_type.clone()),
                    copilot,
                },
            );

            save_settings(&settings);
            let action = if replaced { "updated" } else { "added" };
            println!(
                "{} Provider \"{}\" (type: {}) {action}.",
                "✓".green().bold(),
                provider_id.green(),
                provider_type.display_name()
            );

            // Authenticate if OAuth (copilot)
            if provider_type == ProviderType::Copilot {
                println!();
                println!("{}", "Authenticating with GitHub Copilot...".bold());
                let provider_proxy = settings
                    .providers
                    .get(&provider_id)
                    .map(|cfg| cfg.proxy.as_str())
                    .unwrap_or("");
                let client = match build_oauth_http_client(&settings, provider_proxy) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("{} Failed to build HTTP client: {e}", "✗".red().bold());
                        if !settings.http.extra_ca_certs.is_empty() {
                            eprintln!(
                                "  hint: check that http.extra_ca_certs entries are readable PEM files"
                            );
                        }
                        return;
                    }
                };
                match claude_proxy_providers::copilot::auth::CopilotAuth::new(client, "vscode")
                    .await
                {
                    Ok(auth) => {
                        if let Err(e) = auth.run_device_flow().await {
                            eprintln!("{} Authentication failed: {e}", "✗".red().bold());
                            if matches!(e, claude_proxy_providers::ProviderError::Network(_)) {
                                let cfg_path = claude_proxy_config::Settings::config_file_path()
                                    .map(|p| p.display().to_string())
                                    .unwrap_or_else(|| "~/.config/claude-proxy/config.toml".into());
                                let err_text = e.to_string();
                                if err_text.contains("dns error")
                                    || err_text.contains("lookup address")
                                {
                                    eprintln!(
                                        "  hint: DNS lookup failed before TLS. Check your DNS/network,\n        or set this provider's proxy in {cfg_path}\n        example: proxy = \"http://127.0.0.1:7890\""
                                    );
                                } else {
                                    eprintln!(
                                        "  hint: if your network performs TLS interception (Fortinet,\n        Zscaler, ...), add the corporate root CA path to\n        http.extra_ca_certs in {cfg_path}\n        example: extra_ca_certs = [\"/etc/ssl/certs/ca-certificates.crt\"]"
                                    );
                                }
                            }
                            return;
                        }
                        let _ = auth.refresh_copilot_token().await;
                        println!("{} Copilot authentication successful!", "✓".green().bold());
                    }
                    Err(e) => {
                        eprintln!("{} Auth init failed: {e}", "✗".red().bold());
                        return;
                    }
                }
            }

            // Try to fetch models and let user pick
            let provider_config = settings.providers.get(&provider_id).unwrap();
            println!("Fetching available models...");
            match claude_proxy_providers::create_provider(&provider_id, provider_config, &settings)
                .await
            {
                Ok(provider) => match provider.list_models().await {
                    Ok(models) if !models.is_empty() => {
                        let model_names: Vec<String> =
                            models.iter().map(|m| m.model_id.clone()).collect();
                        println!();
                        let selection = dialoguer::Select::new()
                            .with_prompt("Choose default model")
                            .items(&model_names)
                            .default(0)
                            .interact()
                            .unwrap();
                        let model_name = &model_names[selection];
                        let model_ref = format!("{provider_id}/{model_name}");
                        settings.model.default = model_ref.clone();
                        save_settings(&settings);
                        println!("  → Default model: {}", model_ref.cyan());
                        return;
                    }
                    _ => {
                        println!("  {} Could not fetch models.", "⚠".yellow());
                    }
                },
                Err(e) => {
                    println!("  {} Provider init failed: {e}", "⚠".yellow());
                }
            }

            // Fallback: ask for model name
            let model_name: String = dialoguer::Input::new()
                .with_prompt("Default model name")
                .default(provider_type.default_model_name().to_string())
                .interact_text()
                .unwrap();
            let model_ref = if model_name.is_empty() {
                format!("{provider_id}/default")
            } else {
                format!("{provider_id}/{model_name}")
            };
            settings.model.default = model_ref.clone();
            save_settings(&settings);
            println!("  → Default model: {}", model_ref.cyan());
        }
        ProviderAction::Edit { id } => {
            let mut settings = load_settings_or_exit();
            let Some(provider) = settings.providers.get(&id).cloned() else {
                eprintln!("{} Provider \"{}\" not found.", "Error:".red().bold(), id);
                process::exit(1);
            };

            let field = dialoguer::Select::new()
                .with_prompt("Edit which field?")
                .items(&["API Key", "Base URL", "Proxy"])
                .interact()
                .unwrap();

            match field {
                0 => {
                    let new_key: String = dialoguer::Password::new()
                        .with_prompt("New API Key")
                        .interact()
                        .unwrap();
                    settings.providers.get_mut(&id).unwrap().api_key = new_key;
                }
                1 => {
                    let new_url: String = dialoguer::Input::new()
                        .with_prompt("New Base URL")
                        .default(provider.base_url.clone())
                        .interact_text()
                        .unwrap();
                    settings.providers.get_mut(&id).unwrap().base_url = new_url;
                }
                2 => {
                    let new_proxy: String = dialoguer::Input::new()
                        .with_prompt("New Proxy")
                        .default(provider.proxy.clone())
                        .interact_text()
                        .unwrap();
                    settings.providers.get_mut(&id).unwrap().proxy = new_proxy;
                }
                _ => unreachable!(),
            }

            save_settings(&settings);
            println!(
                "{} Provider \"{}\" updated.",
                "✓".green().bold(),
                id.green()
            );
        }
        ProviderAction::Delete { id } => {
            let mut settings = load_settings_or_exit();
            if settings.providers.remove(&id).is_some() {
                save_settings(&settings);
                println!("{} Provider \"{}\" deleted.", "✓".green().bold(), id);
            } else {
                eprintln!("{} Provider \"{}\" not found.", "Error:".red().bold(), id);
                process::exit(1);
            }
        }
        ProviderAction::Switch { id } => {
            let mut settings = load_settings_or_exit();
            if !settings.providers.contains_key(&id) {
                eprintln!("{} Provider \"{}\" not found.", "Error:".red().bold(), id);
                process::exit(1);
            }
            let model_name = settings
                .providers
                .get(&id)
                .and_then(|cfg| {
                    let pt = cfg.resolve_type(&id);
                    let m = pt.default_model_name();
                    if m.is_empty() {
                        None
                    } else {
                        Some(m.to_string())
                    }
                })
                .unwrap_or_else(|| "default".to_string());
            let model_ref = format!("{id}/{model_name}");
            settings.model.default = model_ref.clone();
            save_settings(&settings);
            println!(
                "{} Default model set to \"{}\"",
                "✓".green().bold(),
                model_ref.cyan()
            );
        }
        ProviderAction::Test { id } => {
            let settings = load_settings_or_exit();
            let Some(provider_config) = settings.providers.get(&id) else {
                eprintln!("{} Provider \"{}\" not found.", "Error:".red().bold(), id);
                process::exit(1);
            };

            println!("Testing provider \"{}\"...", id.yellow());
            println!("  Base URL: {}", provider_config.base_url);

            match claude_proxy_providers::create_provider(&id, provider_config, &settings).await {
                Ok(provider) => match provider.list_models().await {
                    Ok(_) => {
                        println!("  {} Provider is working", "✓".green());
                    }
                    Err(e) => {
                        println!("  {} Model list failed: {e}", "✗".red());
                    }
                },
                Err(e) => {
                    println!("  {} Provider init failed: {e}", "✗".red());
                }
            }
        }
        ProviderAction::Speedtest { id } => {
            let settings = load_settings_or_exit();
            let Some(provider_config) = settings.providers.get(&id) else {
                eprintln!("{} Provider \"{}\" not found.", "Error:".red().bold(), id);
                process::exit(1);
            };

            println!("Speed testing provider \"{}\"...", id.yellow());

            let start = std::time::Instant::now();
            let result =
                claude_proxy_providers::create_provider(&id, provider_config, &settings).await;
            let elapsed = start.elapsed();

            match result {
                Ok(provider) => {
                    let model_start = std::time::Instant::now();
                    match provider.list_models().await {
                        Ok(_) => {
                            let latency = model_start.elapsed();
                            println!(
                                "  {} Latency: {:.0}ms",
                                "✓".green(),
                                latency.as_secs_f64() * 1000.0
                            );
                        }
                        Err(e) => {
                            println!(
                                "  {} Model list failed ({:.0}ms): {e}",
                                "✗".red(),
                                elapsed.as_secs_f64() * 1000.0
                            );
                        }
                    }
                }
                Err(e) => {
                    println!(
                        "  {} Provider init failed ({:.0}ms): {e}",
                        "✗".red(),
                        elapsed.as_secs_f64() * 1000.0
                    );
                }
            }
        }
        ProviderAction::FetchModels { id } => {
            let settings = load_settings_or_exit();
            let Some(provider_config) = settings.providers.get(&id) else {
                eprintln!("{} Provider \"{}\" not found.", "Error:".red().bold(), id);
                process::exit(1);
            };

            println!("Fetching models from \"{}\"...", id.yellow());
            match claude_proxy_providers::create_provider(&id, provider_config, &settings).await {
                Ok(provider) => match provider.list_models().await {
                    Ok(models) => {
                        println!("  {} Found {} models", "✓".green(), models.len());
                        if !models.is_empty() {
                            for m in &models {
                                println!("    - {}", m.model_id);
                            }
                        }
                    }
                    Err(e) => {
                        println!("  {} Failed: {e}", "✗".red());
                    }
                },
                Err(e) => {
                    println!("  {} Provider init failed: {e}", "✗".red());
                }
            }
        }
    }
}

async fn handle_config(action: ConfigAction) {
    match action {
        ConfigAction::Show => {
            let settings = load_settings_or_exit();
            println!("{}", settings.to_toml());
        }
        ConfigAction::Edit => {
            let path = config_path_or_exit();
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            process::Command::new(&editor)
                .arg(&path)
                .status()
                .expect("failed to open editor");
        }
        ConfigAction::Validate => match claude_proxy_config::Settings::config_file_path() {
            Some(path) if path.exists() => match claude_proxy_config::Settings::load(&path) {
                Ok(settings) => match settings.validate() {
                    Ok(()) => println!("{} Configuration is valid.", "✓".green()),
                    Err(e) => {
                        eprintln!("{} Validation failed: {e}", "✗".red());
                        process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("{} Parse error: {e}", "✗".red());
                    process::exit(1);
                }
            },
            _ => {
                eprintln!("{} No config file found.", "✗".red());
                process::exit(1);
            }
        },
        ConfigAction::Path => {
            let path = config_path_or_exit();
            println!("{}", path.display());
        }
        ConfigAction::Export { path } => {
            let settings = load_settings_or_exit();
            let toml = settings.to_toml();
            match path {
                Some(p) => {
                    std::fs::write(&p, &toml).expect("failed to write export file");
                    println!("{} Exported to {}", "✓".green(), p.display());
                }
                None => print!("{toml}"),
            }
        }
        ConfigAction::Import { path } => {
            let settings = claude_proxy_config::Settings::load(&path).unwrap_or_else(|e| {
                eprintln!("{} Failed to import: {e}", "✗".red());
                process::exit(1);
            });
            save_settings(&settings);
            println!("{} Imported from {}", "✓".green(), path.display());
        }
    }
}

async fn handle_server(action: ServerAction) {
    match action {
        ServerAction::Start { daemon } => {
            let settings = load_settings_or_exit();
            if let Err(e) = settings.validate() {
                eprintln!("{} Config validation failed: {e}", "Error:".red().bold());
                process::exit(1);
            }

            if daemon {
                println!("Starting claude-proxy in daemon mode...");
            } else {
                println!(
                    "{} Starting claude-proxy on {}:{}...",
                    "▸".green().bold(),
                    settings.server.host,
                    settings.server.port
                );
            }

            if let Err(e) = claude_proxy_server::run(settings).await {
                eprintln!("{} Server error: {e}", "Error:".red().bold());
                cleanup_pid_file();
                process::exit(1);
            }

            cleanup_pid_file();
        }
        ServerAction::Stop => {
            #[cfg(unix)]
            {
                match read_pid_file() {
                    Some(pid) => {
                        if is_process_running(pid) {
                            println!("Stopping claude-proxy (PID {pid})...");
                            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
                            // Wait for process to exit
                            for _ in 0..30 {
                                if !is_process_running(pid) {
                                    println!("{} Stopped.", "✓".green());
                                    cleanup_pid_file();
                                    return;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            }
                            eprintln!(
                                "{} Process did not stop within 3 seconds.",
                                "Warning:".yellow()
                            );
                        } else {
                            println!(
                                "{} No running daemon found (stale PID file).",
                                "Warning:".yellow()
                            );
                            cleanup_pid_file();
                        }
                    }
                    None => {
                        eprintln!(
                            "{} No PID file found. Is the daemon running?",
                            "Error:".red().bold()
                        );
                        process::exit(1);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                eprintln!("{}", "Daemon stop is only supported on Unix.".yellow());
            }
        }
        ServerAction::Restart => {
            #[cfg(unix)]
            {
                match read_pid_file() {
                    Some(pid) => {
                        if is_process_running(pid) {
                            println!(
                                "Sending SIGUSR1 to claude-proxy (PID {pid}) for graceful reload..."
                            );
                            unsafe { libc::kill(pid as i32, libc::SIGUSR1) };
                            println!("{} Reload signal sent.", "✓".green());
                        } else {
                            eprintln!(
                                "{} No running daemon found (stale PID file).",
                                "Error:".red().bold()
                            );
                            cleanup_pid_file();
                            process::exit(1);
                        }
                    }
                    None => {
                        eprintln!(
                            "{} No PID file found. Is the daemon running?",
                            "Error:".red().bold()
                        );
                        process::exit(1);
                    }
                }
            }
            #[cfg(not(unix))]
            {
                eprintln!("{}", "Graceful restart is only supported on Unix.".yellow());
            }
        }
        ServerAction::Status => match read_pid_file() {
            Some(pid) => {
                if is_process_running(pid) {
                    println!("{} claude-proxy is running (PID {pid})", "✓".green());
                } else {
                    println!("{} claude-proxy is not running (stale PID file)", "✗".red());
                    cleanup_pid_file();
                }
            }
            None => {
                println!("{} claude-proxy is not running (no PID file)", "✗".red());
            }
        },
    }
}

/// Build the reqwest client used by the interactive Copilot OAuth device
/// flow. Honours `http.connect_timeout`, `http.read_timeout`, and
/// provider proxy, `http.extra_ca_certs`, and HTTP timeouts so the CLI behaves
/// the same as the server-side providers.
fn build_oauth_http_client(
    settings: &claude_proxy_config::Settings,
    proxy: &str,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .hickory_dns(true)
        .connect_timeout(std::time::Duration::from_secs(
            settings.http.connect_timeout,
        ))
        .read_timeout(std::time::Duration::from_secs(settings.http.read_timeout));

    if !proxy.trim().is_empty() {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy).map_err(|e| format!("invalid proxy \"{proxy}\": {e}"))?,
        );
    }

    builder = claude_proxy_providers::apply_extra_ca_certs(builder, &settings.http.extra_ca_certs)
        .map_err(|e| e.to_string())?;

    builder
        .build()
        .map_err(|e| claude_proxy_providers::fmt_reqwest_err(&e))
}

fn load_settings_or_exit() -> claude_proxy_config::Settings {
    // Try auto-migration first
    match claude_proxy_config::migrate::auto_migrate() {
        Ok(Some(settings)) => {
            eprintln!("{} Migrated .env to config.toml", "✓".green());
            return settings;
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("{} Migration error: {e}", "Warning:".yellow());
        }
    }

    match claude_proxy_config::Settings::config_file_path() {
        Some(path) if path.exists() => {
            claude_proxy_config::Settings::load(&path).unwrap_or_else(|e| {
                eprintln!("{} Failed to load config: {e}", "Error:".red().bold());
                process::exit(1);
            })
        }
        _ => {
            eprintln!(
                "{} No config file found. Run `claude-proxy provider add` to get started.",
                "Error:".red().bold()
            );
            process::exit(1);
        }
    }
}

fn save_settings(settings: &claude_proxy_config::Settings) {
    let path = claude_proxy_config::Settings::config_file_path()
        .expect("could not determine config directory");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("failed to create config directory");
    }
    std::fs::write(&path, settings.to_toml()).expect("failed to write config file");
}

fn config_path_or_exit() -> PathBuf {
    claude_proxy_config::Settings::config_file_path()
        .filter(|p| p.exists())
        .unwrap_or_else(|| {
            eprintln!("{} No config file found.", "Error:".red().bold());
            process::exit(1);
        })
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        "***".to_string()
    } else {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    }
}

fn pid_file_path() -> Option<PathBuf> {
    claude_proxy_config::Settings::config_dir().map(|p| p.join("claude-proxy.pid"))
}

fn cleanup_pid_file() {
    if let Some(path) = pid_file_path() {
        let _ = std::fs::remove_file(&path);
    }
}

fn read_pid_file() -> Option<u32> {
    let path = pid_file_path()?;
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse().ok()
}

#[cfg(unix)]
fn is_process_running(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_process_running(_pid: u32) -> bool {
    false
}

fn prompt_provider_type() -> ProviderType {
    let known_types = ProviderType::known_types();
    let type_names: Vec<String> = known_types
        .iter()
        .map(|t| t.display_name().to_string())
        .collect();
    let type_idx = dialoguer::Select::new()
        .with_prompt("Provider type")
        .items(&type_names)
        .default(0)
        .interact()
        .unwrap();
    known_types[type_idx].clone()
}

fn is_custom_provider_type(provider_type: &ProviderType) -> bool {
    matches!(
        provider_type,
        ProviderType::Custom(_) | ProviderType::CustomAnthropic(_)
    )
}
