pub mod app;
pub mod pages;
pub mod theme;
pub mod ui;
pub mod widgets;

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use serde_json::{Map, Value};

use app::{
    App, ConfirmAction, ConfirmKind, ConfirmOverlay, EditableSection, FetchResult, Focus,
    InputAction, InputOverlay, LiveMetrics, LiveModelMetrics, LoadingOverlay, NavItem,
    OAuthOverlay, OAuthResult, OAuthStep, Overlay, PickerAction, PickerOverlay, ProviderCheckOk,
    ProviderCheckResult, ProviderCheckStatus, ProviderField, ProviderFocus, StoredMetrics, Toast,
};
use claude_proxy_config::Settings;
use claude_proxy_config::settings::{CopilotProviderConfig, ProviderConfig, ProviderType};
use tracing::{error, info};

const TICK_RATE: Duration = Duration::from_millis(200);
/// Fetch metrics every 5 seconds (25 ticks * 200ms).
const METRICS_FETCH_INTERVAL: u64 = 25;

pub fn run() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let settings = load_settings();
    let result = run_app(&mut terminal, settings);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

fn load_settings() -> Settings {
    match Settings::config_file_path() {
        Some(path) if path.exists() => Settings::load(&path).unwrap_or_else(|e| {
            eprintln!("Warning: failed to load config, using defaults: {e}");
            Settings::default()
        }),
        _ => Settings::default(),
    }
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, settings: Settings) -> anyhow::Result<()>
where
    <B as Backend>::Error: Send + Sync + 'static,
{
    let mut app = App::new(settings);
    let mut last_tick = Instant::now();

    loop {
        // Clamp content index
        app.clamp_content_idx();

        // Poll background fetch results
        poll_fetch(&mut app);
        // Poll background metrics results
        poll_metrics(&mut app);

        // Render
        terminal.draw(|f| ui::render(f, &app))?;

        // Tick
        let elapsed = last_tick.elapsed();
        if elapsed >= TICK_RATE {
            app.tick = app.tick.wrapping_add(1);
            on_tick(&mut app);
            last_tick = Instant::now();
        }

        // Poll events with timeout
        let timeout = TICK_RATE.saturating_sub(elapsed);
        if event::poll(timeout)?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            handle_key(&mut app, key);
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn on_tick(app: &mut App) {
    // Expire toast
    if let Some(ref mut toast) = app.toast {
        if toast.remaining_ticks > 0 {
            toast.remaining_ticks -= 1;
        }
        if toast.remaining_ticks == 0 {
            app.toast = None;
        }
    }
    // Advance loading spinner
    if let Some(Overlay::Loading(ref mut loading)) = app.overlay {
        loading.spinner_tick = app.tick;
    }
    // Advance OAuth spinner
    if let Some(Overlay::OAuth(ref mut oauth)) = app.overlay {
        oauth.spinner_tick = app.tick;
    }
    // Periodically fetch metrics from the running server
    app.metrics_fetch_tick += 1;
    if app.metrics_fetch_tick >= METRICS_FETCH_INTERVAL {
        app.metrics_fetch_tick = 0;
        fetch_live_metrics(app);
    }
    // Poll OAuth results
    poll_oauth(app);
    // Poll provider connectivity/auth checks
    poll_provider_check(app);
}

fn handle_key(app: &mut App, key: event::KeyEvent) {
    // Ctrl+C always quits immediately
    if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
        app.should_quit = true;
        return;
    }

    // Ctrl+S saves anywhere
    if is_ctrl_key(key, 's') {
        if apply_pending_input(app) {
            save_settings_with_feedback(app);
        }
        return;
    }

    // Overlay mode — handle overlay keys first
    if app.overlay.is_some() {
        handle_overlay_key(app, key);
        return;
    }

    // Vim-style hjkl remapping (not in all contexts to avoid breaking input)
    let code = match key.code {
        KeyCode::Char('h') if key.modifiers.is_empty() => KeyCode::Left,
        KeyCode::Char('j') if key.modifiers.is_empty() => KeyCode::Down,
        KeyCode::Char('k') if key.modifiers.is_empty() => KeyCode::Up,
        KeyCode::Char('l') if key.modifiers.is_empty() => KeyCode::Right,
        _ => key.code,
    };

    // Normalize Ctrl+H → Backspace
    let code = if key.modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('h') {
        KeyCode::Backspace
    } else {
        code
    };

    match app.focus {
        Focus::Nav => handle_nav_key(app, code),
        Focus::Content => handle_content_key(app, code, key),
        Focus::Overlay => {} // handled above
    }
}

fn is_ctrl_key(key: event::KeyEvent, c: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(c)
}

fn apply_pending_input(app: &mut App) -> bool {
    let Some(Overlay::Input(input)) = app.overlay.as_ref() else {
        return true;
    };

    let value = input.value.clone();
    let action = input.action.clone();
    app.overlay = None;
    app.focus = Focus::Content;
    apply_input_action(app, &action, &value)
}

fn handle_nav_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Up => {
            app.nav_idx = app.nav_idx.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = NavItem::ALL.len().saturating_sub(1);
            if app.nav_idx < max {
                app.nav_idx += 1;
            }
        }
        KeyCode::Enter => {
            app.nav = NavItem::ALL[app.nav_idx];
            app.focus = Focus::Content;
            app.content_idx = 0;
        }
        KeyCode::Right => {
            app.focus = Focus::Content;
            app.content_idx = 0;
        }
        KeyCode::Char(' ') => {
            app.nav = NavItem::ALL[app.nav_idx];
            app.focus = Focus::Content;
            app.content_idx = 0;
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            if app.nav == NavItem::Dashboard {
                request_quit(app);
            } else {
                app.nav = NavItem::Dashboard;
                app.nav_idx = 0;
            }
        }
        KeyCode::Char('?') => {
            app.overlay = Some(Overlay::Help);
            app.focus = Focus::Overlay;
        }
        _ => {}
    }
}

fn handle_content_key(app: &mut App, code: KeyCode, _key: event::KeyEvent) {
    // Providers page has two-level navigation: List ↔ Detail
    if app.nav == NavItem::Providers {
        handle_providers_key(app, code);
        return;
    }

    match code {
        KeyCode::Up => {
            app.content_idx = app.content_idx.saturating_sub(1);
        }
        KeyCode::Down => {
            let max = app.item_count().saturating_sub(1);
            if app.content_idx < max {
                app.content_idx += 1;
            }
        }
        KeyCode::Left => {
            app.focus = Focus::Nav;
        }
        KeyCode::Enter | KeyCode::Char('e')
            if app.item_count() > 0 && app.content_idx < app.item_count() =>
        {
            start_editing(app);
        }
        KeyCode::Char(' ') => {
            handle_toggle(app);
        }
        // Navigation
        KeyCode::Esc => {
            if app.nav == NavItem::Dashboard {
                request_quit(app);
            } else {
                app.nav = NavItem::Dashboard;
                app.nav_idx = 0;
                app.focus = Focus::Nav;
            }
        }
        KeyCode::Char('q') => {
            request_quit(app);
        }
        KeyCode::Char('?') => {
            app.overlay = Some(Overlay::Help);
            app.focus = Focus::Overlay;
        }
        _ => {}
    }
}

fn handle_providers_key(app: &mut App, code: KeyCode) {
    match app.provider_focus {
        ProviderFocus::List => match code {
            KeyCode::Up => {
                app.content_idx = app.content_idx.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = app.item_count().saturating_sub(1);
                if app.content_idx < max {
                    app.content_idx += 1;
                }
            }
            KeyCode::Right | KeyCode::Enter if !app.settings.providers.is_empty() => {
                app.provider_focus = ProviderFocus::Detail;
                app.detail_idx = 0;
            }
            KeyCode::Left => {
                app.focus = Focus::Nav;
            }
            KeyCode::Char('a') => {
                add_provider(app);
            }
            KeyCode::Char('d') => {
                delete_provider(app);
            }
            KeyCode::Char('o') => {
                oauth_provider(app);
            }
            KeyCode::Char('t') => {
                check_selected_provider(app);
            }
            KeyCode::Esc => {
                app.focus = Focus::Nav;
            }
            KeyCode::Char('q') => {
                request_quit(app);
            }
            KeyCode::Char('?') => {
                app.overlay = Some(Overlay::Help);
                app.focus = Focus::Overlay;
            }
            _ => {}
        },
        ProviderFocus::Detail => match code {
            KeyCode::Up => {
                app.detail_idx = app.detail_idx.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = app.provider_detail_field_count().saturating_sub(1);
                if app.detail_idx < max {
                    app.detail_idx += 1;
                }
            }
            KeyCode::Left | KeyCode::Esc => {
                app.provider_focus = ProviderFocus::List;
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                start_editing(app);
            }
            KeyCode::Char('a') => {
                add_provider(app);
            }
            KeyCode::Char('d') => {
                delete_provider(app);
            }
            KeyCode::Char('o') => {
                oauth_provider(app);
            }
            KeyCode::Char('t') => {
                check_selected_provider(app);
            }
            KeyCode::Char('q') => {
                request_quit(app);
            }
            KeyCode::Char('?') => {
                app.overlay = Some(Overlay::Help);
                app.focus = Focus::Overlay;
            }
            _ => {}
        },
    }
}

fn handle_overlay_key(app: &mut App, key: event::KeyEvent) {
    if matches!(app.overlay, Some(Overlay::OAuth(_))) {
        handle_oauth_overlay_key(app, key);
        return;
    }

    match app.overlay.as_mut().unwrap() {
        Overlay::Confirm(overlay) => {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let kind = overlay.kind.clone();
                    app.overlay = None;
                    app.focus = Focus::Nav;
                    match kind {
                        ConfirmKind::YesNo { on_yes } => match on_yes {
                            ConfirmAction::Quit => app.should_quit = true,
                            ConfirmAction::DeleteProvider(id) => {
                                app.settings.providers.remove(&id);
                                app.provider_statuses.remove(&id);
                                app.mark_dirty();
                                app.clamp_content_idx();
                                app.show_toast(Toast::success(format!(
                                    "Provider \"{id}\" deleted"
                                )));
                            }
                            ConfirmAction::SaveAndQuit => {
                                if save_settings_with_feedback(app) {
                                    app.should_quit = true;
                                }
                            }
                        },
                        ConfirmKind::DirtyQuit => {
                            // Enter = Save & Quit
                            if save_settings_with_feedback(app) {
                                app.should_quit = true;
                            }
                        }
                        ConfirmKind::Info => {}
                    }
                }
                KeyCode::Char('n') | KeyCode::Char('N') => {
                    let kind = &overlay.kind;
                    match kind {
                        ConfirmKind::DirtyQuit => {
                            // Discard and quit
                            app.overlay = None;
                            app.focus = Focus::Nav;
                            app.should_quit = true;
                        }
                        _ => {
                            app.overlay = None;
                            app.focus = Focus::Nav;
                        }
                    }
                }
                KeyCode::Esc => {
                    app.overlay = None;
                    app.focus = Focus::Nav;
                }
                _ => {}
            }
        }
        Overlay::Input(input) => match key.code {
            KeyCode::Enter => {
                apply_pending_input(app);
            }
            KeyCode::Esc => {
                app.overlay = None;
                app.focus = Focus::Content;
            }
            KeyCode::Backspace => {
                input.backspace();
            }
            KeyCode::Char(c) => {
                input.insert(c);
            }
            _ => {}
        },
        Overlay::Picker(picker) => {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    let max = picker.items.len().saturating_sub(1);
                    if picker.selected < max {
                        picker.selected += 1;
                    }
                }
                KeyCode::Enter => {
                    let selected = picker
                        .items
                        .get(picker.selected)
                        .cloned()
                        .unwrap_or_default();
                    let action = picker.action.clone();
                    match action {
                        PickerAction::SetModelDefault { provider_id } => {
                            app.overlay = None;
                            app.focus = Focus::Content;
                            app.settings.model.default = format!("{provider_id}/{selected}");
                            app.mark_dirty();
                            app.show_toast(Toast::success(format!(
                                "Default model: {provider_id}/{selected}"
                            )));
                        }
                        PickerAction::PickProviderForModel { section } => {
                            // Step 1 complete: fetch models for the selected provider
                            app.pending_model_section = Some(section);
                            // Show loading
                            app.overlay = Some(Overlay::Loading(LoadingOverlay {
                                title: format!("Fetching models from {selected}"),
                                message: "Please wait...".into(),
                                spinner_tick: app.tick,
                            }));
                            // Spawn fetch via provider (handles all auth types)
                            let settings = app.settings.clone();
                            let handle = app.tokio_handle.clone();
                            let (tx, rx) = std::sync::mpsc::channel();
                            app.fetch_rx = Some(rx);
                            let pid = selected;
                            std::thread::spawn(move || {
                                let models =
                                    fetch_models_via_provider(&pid, &settings, handle.as_ref());
                                let _ = tx.send(FetchResult {
                                    provider_id: pid,
                                    models,
                                });
                            });
                        }
                        PickerAction::SetModelField {
                            provider_id,
                            section,
                        } => {
                            app.overlay = None;
                            app.focus = Focus::Content;
                            let value = format!("{provider_id}/{selected}");
                            set_model_field(app, &section, &value);
                            app.mark_dirty();
                            app.show_toast(Toast::success(format!(
                                "{} = {}",
                                get_section_label(&section),
                                value
                            )));
                        }
                        PickerAction::SetLogLevel => {
                            app.overlay = None;
                            app.focus = Focus::Content;
                            app.settings.log.level = selected.clone();
                            app.mark_dirty();
                            app.show_toast(Toast::success(format!("Log level: {selected}")));
                        }
                        PickerAction::AddProvider => {
                            let idx = picker.selected;
                            let types = app.pending_provider_types.take().unwrap_or_default();
                            let provider_type = types
                                .into_iter()
                                .nth(idx)
                                .unwrap_or(ProviderType::Custom(String::new()));

                            let default_id = default_provider_id(&provider_type).to_string();
                            let id = default_id;
                            let provider_type = provider_type_with_id(provider_type, &id);

                            let copilot = if provider_type == ProviderType::Copilot {
                                Some(CopilotProviderConfig::default())
                            } else {
                                None
                            };

                            let is_oauth = !provider_type.needs_api_key();

                            let replaced = app.settings.providers.contains_key(&id);
                            app.settings.providers.insert(
                                id.clone(),
                                ProviderConfig {
                                    api_key: String::new(),
                                    base_url: provider_type.default_base_url().to_string(),
                                    proxy: String::new(),
                                    provider_type: Some(provider_type),
                                    copilot,
                                },
                            );
                            app.mark_dirty();
                            app.content_idx = app
                                .settings
                                .providers
                                .keys()
                                .position(|provider_id| provider_id == &id)
                                .unwrap_or_else(|| app.settings.providers.len().saturating_sub(1));
                            app.overlay = None;
                            app.focus = Focus::Content;
                            let action = if replaced { "Updated" } else { "Added" };
                            app.show_toast(Toast::success(format!(
                                "{action} \"{id}\". Edit fields, Ctrl+S to save."
                            )));

                            // Start OAuth flow for providers that need it
                            if is_oauth {
                                start_oauth_flow(app, &id);
                            }
                        }
                    }
                }
                KeyCode::Esc => {
                    app.overlay = None;
                    app.focus = Focus::Content;
                }
                _ => {}
            }
        }
        Overlay::Loading(_) => {
            if key.code == KeyCode::Esc {
                app.overlay = None;
                app.focus = Focus::Content;
                app.fetch_rx = None;
                app.show_toast(Toast::info("Cancelled"));
            }
        }
        Overlay::OAuth(_) => {}
        Overlay::Help => {
            if key.code == KeyCode::Esc || key.code == KeyCode::Char('?') {
                app.overlay = None;
                app.focus = Focus::Nav;
            }
        }
    }
}

fn handle_oauth_overlay_key(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            app.overlay = None;
            app.focus = Focus::Content;
            app.oauth_rx = None;
            app.oauth_pending_id = None;
            app.oauth_device_info = None;
            app.show_toast(Toast::info("OAuth cancelled"));
        }
        KeyCode::Char('c') => copy_oauth_value(app, OAuthCopyTarget::Code),
        KeyCode::Char('u') => copy_oauth_value(app, OAuthCopyTarget::Url),
        _ => {}
    }
}

enum OAuthCopyTarget {
    Code,
    Url,
}

fn copy_oauth_value(app: &mut App, target: OAuthCopyTarget) {
    let Some(Overlay::OAuth(oauth)) = app.overlay.as_ref() else {
        return;
    };

    let OAuthStep::ShowCode { url, code } = &oauth.step else {
        app.show_toast(Toast::info("Device code not ready yet"));
        return;
    };

    let (label, value) = match target {
        OAuthCopyTarget::Code => ("Device code", code.as_str()),
        OAuthCopyTarget::Url => ("Verification URL", url.as_str()),
    };

    match copy_to_clipboard(value) {
        Ok(()) => app.show_toast(Toast::success(format!("{label} copied"))),
        Err(err) => app.show_toast(Toast::error(format!("Copy failed: {err}"))),
    }
}

fn copy_to_clipboard(value: &str) -> io::Result<()> {
    let encoded = general_purpose::STANDARD.encode(value.as_bytes());
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()
}

// ── Actions ──

fn request_quit(app: &mut App) {
    if app.dirty {
        app.overlay = Some(Overlay::Confirm(ConfirmOverlay {
            title: "Unsaved Changes".into(),
            message: "You have unsaved changes. What would you like to do?".into(),
            kind: ConfirmKind::DirtyQuit,
        }));
    } else {
        app.overlay = Some(Overlay::Confirm(ConfirmOverlay {
            title: "Quit".into(),
            message: "Quit claude-proxy configuration?".into(),
            kind: ConfirmKind::YesNo {
                on_yes: ConfirmAction::Quit,
            },
        }));
    }
    app.focus = Focus::Overlay;
}

fn start_editing(app: &mut App) {
    // For provider page: edit the selected provider's field based on detail_idx
    if app.nav == NavItem::Providers {
        if let Some((id, cfg)) = app.settings.providers.iter().nth(app.content_idx) {
            let id = id.clone();
            let pt = cfg.resolve_type(&id);
            let (field, prompt, value) = match app.detail_idx {
                0 => {
                    if !pt.needs_api_key() {
                        app.show_toast(Toast::info(format!(
                            "{} uses OAuth (API key not editable)",
                            pt.display_name()
                        )));
                        return;
                    }
                    (ProviderField::ApiKey, "API Key", cfg.api_key.clone())
                }
                1 => (ProviderField::BaseUrl, "Base URL", cfg.base_url.clone()),
                2 => (ProviderField::Proxy, "Proxy", cfg.proxy.clone()),
                _ => return,
            };
            let cursor = value.len();
            app.overlay = Some(Overlay::Input(InputOverlay {
                title: format!("Edit {id}"),
                prompt: prompt.into(),
                value,
                cursor,
                action: InputAction::EditProviderField {
                    provider_id: id,
                    field,
                    field_index: app.detail_idx,
                },
            }));
            app.focus = Focus::Overlay;
        }
        return;
    }

    // For model page: show provider picker first, then model picker
    if app.nav == NavItem::Model {
        let section = match app.content_idx {
            0 => EditableSection::ModelDefault,
            1 => EditableSection::ModelReasoning,
            2 => EditableSection::ModelOpus,
            3 => EditableSection::ModelSonnet,
            4 => EditableSection::ModelHaiku,
            _ => return,
        };
        // Build provider list for picker
        let providers: Vec<String> = app.settings.providers.keys().cloned().collect();
        if providers.is_empty() {
            app.show_toast(Toast::warning(
                "No providers configured. Add a provider first.",
            ));
            return;
        }
        app.overlay = Some(Overlay::Picker(PickerOverlay {
            title: "Select Provider".into(),
            items: providers,
            selected: 0,
            action: PickerAction::PickProviderForModel { section },
        }));
        app.focus = Focus::Overlay;
        return;
    }

    // For other pages: show text input overlay
    // Special case: Log level uses a picker
    if app.nav == NavItem::Log && app.content_idx == 0 {
        let levels = vec![
            "trace".to_string(),
            "debug".to_string(),
            "info".to_string(),
            "warn".to_string(),
            "error".to_string(),
        ];
        let current_idx = levels
            .iter()
            .position(|l| l == &app.settings.log.level)
            .unwrap_or(2);
        app.overlay = Some(Overlay::Picker(PickerOverlay {
            title: "Select Log Level".into(),
            items: levels,
            selected: current_idx,
            action: PickerAction::SetLogLevel,
        }));
        app.focus = Focus::Overlay;
        return;
    }

    let (section, value) = get_editable_section(app);
    if let Some(section) = section {
        let cursor = value.len();
        app.overlay = Some(Overlay::Input(InputOverlay {
            title: format!("Edit {}", app.nav.name()),
            prompt: get_section_label(&section).into(),
            value,
            cursor,
            action: InputAction::EditSetting { section },
        }));
        app.focus = Focus::Overlay;
    }
}

fn get_editable_section(app: &App) -> (Option<EditableSection>, String) {
    match app.nav {
        NavItem::Server => match app.content_idx {
            0 => (
                Some(EditableSection::ServerHost),
                app.settings.server.host.clone(),
            ),
            1 => (
                Some(EditableSection::ServerPort),
                app.settings.server.port.to_string(),
            ),
            2 => (
                Some(EditableSection::ServerAuthToken),
                app.settings.server.auth_token.clone(),
            ),
            3 => (
                Some(EditableSection::AdminAuthToken),
                app.settings.admin.auth_token.clone().unwrap_or_default(),
            ),
            _ => (None, String::new()),
        },
        NavItem::Limits => match app.content_idx {
            0 => (
                Some(EditableSection::RateLimit),
                app.settings.limits.rate_limit.to_string(),
            ),
            1 => (
                Some(EditableSection::RateWindow),
                app.settings.limits.rate_window.to_string(),
            ),
            2 => (
                Some(EditableSection::MaxConcurrency),
                app.settings.limits.max_concurrency.to_string(),
            ),
            _ => (None, String::new()),
        },
        NavItem::Http => match app.content_idx {
            0 => (
                Some(EditableSection::HttpReadTimeout),
                app.settings.http.read_timeout.to_string(),
            ),
            1 => (
                Some(EditableSection::HttpWriteTimeout),
                app.settings.http.write_timeout.to_string(),
            ),
            2 => (
                Some(EditableSection::HttpConnectTimeout),
                app.settings.http.connect_timeout.to_string(),
            ),
            _ => (None, String::new()),
        },
        NavItem::Log => match app.content_idx {
            0 => (
                Some(EditableSection::LogLevel),
                app.settings.log.level.clone(),
            ),
            _ => (None, String::new()),
        },
        NavItem::Model => match app.content_idx {
            0 => (
                Some(EditableSection::ModelDefault),
                app.settings.model.default.clone(),
            ),
            1 => (
                Some(EditableSection::ModelReasoning),
                app.settings.model.reasoning.clone().unwrap_or_default(),
            ),
            2 => (
                Some(EditableSection::ModelOpus),
                app.settings.model.opus.clone().unwrap_or_default(),
            ),
            3 => (
                Some(EditableSection::ModelSonnet),
                app.settings.model.sonnet.clone().unwrap_or_default(),
            ),
            4 => (
                Some(EditableSection::ModelHaiku),
                app.settings.model.haiku.clone().unwrap_or_default(),
            ),
            _ => (None, String::new()),
        },
        _ => (None, String::new()),
    }
}

fn get_section_label(section: &EditableSection) -> &'static str {
    match section {
        EditableSection::ServerHost => "Host",
        EditableSection::ServerPort => "Port",
        EditableSection::ServerAuthToken => "Auth Token",
        EditableSection::AdminAuthToken => "Admin Auth Token",
        EditableSection::RateLimit => "Rate Limit (requests)",
        EditableSection::RateWindow => "Window (seconds)",
        EditableSection::MaxConcurrency => "Max Concurrency",
        EditableSection::HttpReadTimeout => "Read Timeout (seconds)",
        EditableSection::HttpWriteTimeout => "Write Timeout (seconds)",
        EditableSection::HttpConnectTimeout => "Connect Timeout (seconds)",
        EditableSection::LogLevel => "Log Level",
        EditableSection::ModelDefault => "Default Model",
        EditableSection::ModelReasoning => "Reasoning Model",
        EditableSection::ModelOpus => "Opus Alias",
        EditableSection::ModelSonnet => "Sonnet Alias",
        EditableSection::ModelHaiku => "Haiku Alias",
    }
}

fn apply_input_action(app: &mut App, action: &InputAction, value: &str) -> bool {
    match action {
        InputAction::EditSetting { section } => {
            let v = value.to_string();
            match section {
                EditableSection::ServerHost => app.settings.server.host = v,
                EditableSection::ServerPort => {
                    if let Ok(p) = v.parse() {
                        app.settings.server.port = p;
                    } else {
                        app.show_toast(Toast::error("Invalid port"));
                        return false;
                    }
                }
                EditableSection::ServerAuthToken => app.settings.server.auth_token = v,
                EditableSection::AdminAuthToken => {
                    app.settings.admin.auth_token = if v.is_empty() { None } else { Some(v) };
                }
                EditableSection::RateLimit => {
                    if let Ok(n) = v.parse() {
                        app.settings.limits.rate_limit = n;
                    } else {
                        app.show_toast(Toast::error("Invalid number"));
                        return false;
                    }
                }
                EditableSection::RateWindow => {
                    if let Ok(n) = v.parse() {
                        app.settings.limits.rate_window = n;
                    } else {
                        app.show_toast(Toast::error("Invalid number"));
                        return false;
                    }
                }
                EditableSection::MaxConcurrency => {
                    if let Ok(n) = v.parse() {
                        app.settings.limits.max_concurrency = n;
                    } else {
                        app.show_toast(Toast::error("Invalid number"));
                        return false;
                    }
                }
                EditableSection::HttpReadTimeout => {
                    if let Ok(n) = v.parse() {
                        app.settings.http.read_timeout = n;
                    } else {
                        app.show_toast(Toast::error("Invalid number"));
                        return false;
                    }
                }
                EditableSection::HttpWriteTimeout => {
                    if let Ok(n) = v.parse() {
                        app.settings.http.write_timeout = n;
                    } else {
                        app.show_toast(Toast::error("Invalid number"));
                        return false;
                    }
                }
                EditableSection::HttpConnectTimeout => {
                    if let Ok(n) = v.parse() {
                        app.settings.http.connect_timeout = n;
                    } else {
                        app.show_toast(Toast::error("Invalid number"));
                        return false;
                    }
                }
                EditableSection::LogLevel => app.settings.log.level = v,
                EditableSection::ModelDefault => app.settings.model.default = v,
                EditableSection::ModelReasoning => {
                    app.settings.model.reasoning = if v.is_empty() { None } else { Some(v) };
                }
                EditableSection::ModelOpus => {
                    app.settings.model.opus = if v.is_empty() { None } else { Some(v) };
                }
                EditableSection::ModelSonnet => {
                    app.settings.model.sonnet = if v.is_empty() { None } else { Some(v) };
                }
                EditableSection::ModelHaiku => {
                    app.settings.model.haiku = if v.is_empty() { None } else { Some(v) };
                }
            }
            app.mark_dirty();
            app.show_toast(Toast::success("Updated"));
            true
        }
        InputAction::SetModelDefault { provider_id } => {
            app.settings.model.default = format!("{provider_id}/{value}");
            app.mark_dirty();
            app.show_toast(Toast::success(format!(
                "Default model: {provider_id}/{value}"
            )));
            true
        }
        InputAction::EditProviderField {
            provider_id, field, ..
        } => {
            if let Some(cfg) = app.settings.providers.get_mut(provider_id) {
                match field {
                    ProviderField::ApiKey => cfg.api_key = value.to_string(),
                    ProviderField::BaseUrl => cfg.base_url = value.to_string(),
                    ProviderField::Proxy => cfg.proxy = value.to_string(),
                }
                app.provider_statuses.remove(provider_id);
                app.mark_dirty();
                app.show_toast(Toast::success("Updated"));
                true
            } else {
                false
            }
        }
    }
}

fn handle_toggle(app: &mut App) {
    if app.nav == NavItem::Log {
        match app.content_idx {
            1 => {
                app.settings.log.raw_api_payloads = !app.settings.log.raw_api_payloads;
                app.mark_dirty();
                app.show_toast(Toast::info(format!(
                    "Raw API payloads: {}",
                    if app.settings.log.raw_api_payloads {
                        "ON"
                    } else {
                        "OFF"
                    }
                )));
            }
            2 => {
                app.settings.log.raw_sse_events = !app.settings.log.raw_sse_events;
                app.mark_dirty();
                app.show_toast(Toast::info(format!(
                    "Raw SSE events: {}",
                    if app.settings.log.raw_sse_events {
                        "ON"
                    } else {
                        "OFF"
                    }
                )));
            }
            _ => {}
        }
    }
}

fn add_provider(app: &mut App) {
    let types = ProviderType::known_types();
    let items: Vec<String> = types
        .iter()
        .map(|t| {
            let desc = match t {
                ProviderType::Copilot => " — OAuth, no API key needed",
                ProviderType::ChatGPT => " — ChatGPT OAuth",
                ProviderType::OpenAI => " — API key",
                ProviderType::Anthropic => " — API key",
                ProviderType::OpenRouter => " — API key, OpenAI-compatible",
                ProviderType::Google => " — API key, OpenAI-compatible",
                ProviderType::Custom(_) => " — OpenAI-compatible",
                ProviderType::CustomAnthropic(_) => " — Anthropic-compatible",
            };
            format!("{}{}", t.display_name(), desc)
        })
        .collect();
    app.pending_provider_types = Some(types);
    app.overlay = Some(Overlay::Picker(PickerOverlay {
        title: "Add Provider — Select Type".into(),
        items,
        selected: 0,
        action: PickerAction::AddProvider,
    }));
    app.focus = Focus::Overlay;
}

fn default_provider_id(provider_type: &ProviderType) -> &str {
    match provider_type {
        ProviderType::Custom(_) => "custom-openai",
        ProviderType::CustomAnthropic(_) => "custom-anthropic",
        _ => provider_type.as_str(),
    }
}

fn provider_type_with_id(provider_type: ProviderType, id: &str) -> ProviderType {
    match provider_type {
        ProviderType::Custom(_) => ProviderType::Custom(id.to_string()),
        ProviderType::CustomAnthropic(_) => ProviderType::CustomAnthropic(id.to_string()),
        other => other,
    }
}

fn check_selected_provider(app: &mut App) {
    let Some((id, cfg)) = app.settings.providers.iter().nth(app.content_idx) else {
        app.show_toast(Toast::info("No provider selected"));
        return;
    };

    if app.provider_check_rx.is_some() {
        app.show_toast(Toast::info("Provider check already running"));
        return;
    }

    let id = id.clone();
    let provider_type = cfg.resolve_type(&id);

    if provider_type.needs_api_key() && cfg.api_key.trim().is_empty() {
        app.provider_statuses.insert(
            id.clone(),
            ProviderCheckStatus::Failed("Missing API key".into()),
        );
        app.show_toast(Toast::error(format!("{id}: missing API key")));
        return;
    }

    if matches!(
        provider_type,
        ProviderType::Anthropic | ProviderType::CustomAnthropic(_)
    ) {
        let message = "Configured; auth not verified by model list".to_string();
        app.provider_statuses
            .insert(id.clone(), ProviderCheckStatus::Warning(message));
        app.show_toast(Toast::warning(format!("{id}: Anthropic auth not verified")));
        return;
    }

    let settings = app.settings.clone();
    let handle = app.tokio_handle.clone();
    let (tx, rx) = std::sync::mpsc::channel();

    app.provider_statuses
        .insert(id.clone(), ProviderCheckStatus::Checking);
    app.provider_check_rx = Some(rx);
    app.show_toast(Toast::info(format!("Checking {id}...")));

    std::thread::spawn(move || {
        let result = fetch_models_via_provider(&id, &settings, handle.as_ref()).map(|models| {
            ProviderCheckOk {
                message: format!("OK, {} models available", models.len()),
            }
        });
        let _ = tx.send(ProviderCheckResult {
            provider_id: id,
            result,
        });
    });
}

/// Fetch models using the provider trait (handles OAuth, API keys, etc. correctly).
fn fetch_models_via_provider(
    provider_id: &str,
    settings: &claude_proxy_config::Settings,
    handle: Option<&tokio::runtime::Handle>,
) -> Result<Vec<String>, String> {
    let Some(cfg) = settings.providers.get(provider_id) else {
        return Err("Provider not found in config".into());
    };

    let handle = handle.ok_or_else(|| {
        error!("No tokio runtime handle available");
        "No async runtime".to_string()
    })?;

    info!("Fetching models for provider={provider_id}");

    let pid = provider_id.to_string();
    let settings_clone = settings.clone();
    let result: Result<Vec<String>, String> = handle.block_on(async {
        let provider = claude_proxy_providers::create_provider(&pid, cfg, &settings_clone)
            .await
            .map_err(|e| {
                error!("Failed to create provider for provider={pid}: {e}");
                format!("Provider init failed: {e}")
            })?;

        let models = provider.list_models().await.map_err(|e| {
            error!("list_models failed for provider={pid}: {e}");
            format!("list_models failed: {e}")
        })?;

        let names: Vec<String> = models.into_iter().map(|m| m.model_id).collect();
        Ok(names)
    });

    match &result {
        Ok(models) => info!("Fetched {} models for provider={provider_id}", models.len()),
        Err(e) => error!("Model fetch failed for provider={provider_id}: {e}"),
    }

    result
}

/// Start the GitHub OAuth device flow for a Copilot provider in the background.
fn start_oauth_flow(app: &mut App, provider_id: &str) {
    let pid = provider_id.to_string();
    let settings = app.settings.clone();
    let provider_type = settings
        .providers
        .get(&pid)
        .map(|cfg| cfg.resolve_type(&pid))
        .unwrap_or_else(|| ProviderType::parse(&pid));
    let (tx, rx) = std::sync::mpsc::channel();
    app.oauth_rx = Some(rx);
    app.oauth_pending_id = Some(pid.clone());
    app.overlay = Some(Overlay::OAuth(OAuthOverlay {
        provider_id: pid.clone(),
        step: OAuthStep::Requesting,
        spinner_tick: app.tick,
    }));
    app.focus = Focus::Overlay;

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("oauth runtime");
        rt.block_on(async {
            let client = match build_tui_oauth_http_client(&settings, &pid) {
                Ok(client) => client,
                Err(e) => {
                    let _ = tx.send(OAuthResult::Error(e));
                    return;
                }
            };

            match provider_type {
                ProviderType::Copilot => {
                    match claude_proxy_providers::copilot::auth::CopilotAuth::new(client, "vscode")
                        .await
                    {
                        Ok(auth) => match auth.start_device_code().await {
                            Ok(info) => {
                                let _ = tx.send(OAuthResult::CodeInfo {
                                    url: info.verification_uri.clone(),
                                    code: info.user_code.clone(),
                                    device_code: info.device_code.clone(),
                                    interval: info.interval,
                                });
                                match auth.complete_device_code(&info).await {
                                    Ok(token) => {
                                        let _ = auth.refresh_copilot_token().await;
                                        let _ = tx.send(OAuthResult::Token(token));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(OAuthResult::Error(e.to_string()));
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(OAuthResult::Error(e.to_string()));
                            }
                        },
                        Err(e) => {
                            let _ = tx.send(OAuthResult::Error(e.to_string()));
                        }
                    }
                }
                ProviderType::ChatGPT => {
                    match claude_proxy_providers::chatgpt::ChatGptAuth::new(client).await {
                        Ok(auth) => match auth.start_device_code().await {
                            Ok(info) => {
                                let _ = tx.send(OAuthResult::CodeInfo {
                                    url: info.verification_uri.clone(),
                                    code: info.user_code.clone(),
                                    device_code: info.device_auth_id.clone(),
                                    interval: info.interval,
                                });
                                match auth.complete_device_code(&info).await {
                                    Ok(token) => {
                                        let _ = tx.send(OAuthResult::Token(token));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(OAuthResult::Error(e.to_string()));
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(OAuthResult::Error(e.to_string()));
                            }
                        },
                        Err(e) => {
                            let _ = tx.send(OAuthResult::Error(e.to_string()));
                        }
                    }
                }
                _ => {
                    let _ = tx.send(OAuthResult::Error(
                        "selected provider does not support OAuth".to_string(),
                    ));
                }
            }
        });
    });
}

fn build_tui_oauth_http_client(
    settings: &Settings,
    provider_id: &str,
) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .hickory_dns(true)
        .connect_timeout(Duration::from_secs(settings.http.connect_timeout))
        .read_timeout(Duration::from_secs(settings.http.read_timeout));

    if let Some(proxy) = settings
        .providers
        .get(provider_id)
        .map(|cfg| cfg.proxy.trim())
        && !proxy.is_empty()
    {
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

/// Poll OAuth background thread results and update overlay state.
fn poll_oauth(app: &mut App) {
    if let Some(ref rx) = app.oauth_rx {
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => {
                OAuthResult::Error("OAuth worker exited before returning a device code".into())
            }
        };

        match result {
            OAuthResult::CodeInfo {
                url,
                code,
                device_code,
                interval,
            } => {
                // Stash device_code + interval for the polling phase
                app.oauth_device_info = Some((device_code, interval));
                // Update the overlay to show URL + code
                if let Some(Overlay::OAuth(ref mut oa)) = app.overlay {
                    oa.step = OAuthStep::ShowCode {
                        url: url.clone(),
                        code: code.clone(),
                    };
                }
            }
            OAuthResult::Token(_) => {
                let provider_id = app
                    .oauth_pending_id
                    .clone()
                    .unwrap_or_else(|| "Provider".to_string());
                app.oauth_rx = None;
                if let Some(Overlay::OAuth(ref mut oa)) = app.overlay {
                    oa.step = OAuthStep::Success;
                }
                // Auto-dismiss after brief delay (handled via tick counter in overlay)
                app.overlay = None;
                app.focus = Focus::Content;
                app.oauth_pending_id = None;
                app.oauth_device_info = None;
                app.show_toast(Toast::success(format!(
                    "{provider_id} authenticated successfully"
                )));
            }
            OAuthResult::Error(err) => {
                app.oauth_rx = None;
                if let Some(Overlay::OAuth(ref mut oa)) = app.overlay {
                    oa.step = OAuthStep::Failed(err.clone());
                }
                // Keep overlay open so user can see the error; Esc to dismiss
            }
        }
    }
}

fn poll_provider_check(app: &mut App) {
    let Some(ref rx) = app.provider_check_rx else {
        return;
    };

    let result = match rx.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty) => return,
        Err(TryRecvError::Disconnected) => {
            app.provider_check_rx = None;
            app.show_toast(Toast::error("Provider check worker exited"));
            return;
        }
    };

    app.provider_check_rx = None;
    match result.result {
        Ok(ok) => {
            app.provider_statuses.insert(
                result.provider_id.clone(),
                ProviderCheckStatus::Ok(ok.message.clone()),
            );
            app.show_toast(Toast::success(format!(
                "{}: {}",
                result.provider_id, ok.message
            )));
        }
        Err(err) => {
            app.provider_statuses.insert(
                result.provider_id.clone(),
                ProviderCheckStatus::Failed(err.clone()),
            );
            app.show_toast(Toast::error(format!(
                "{} check failed: {}",
                result.provider_id, err
            )));
        }
    }
}

fn poll_fetch(app: &mut App) {
    if let Some(ref rx) = app.fetch_rx
        && let Ok(result) = rx.try_recv()
    {
        app.fetch_rx = None;
        let pending_section = app.pending_model_section.take();
        let provider_name = result.provider_id.clone();
        match result.models {
            Ok(models) => {
                let action = match pending_section {
                    Some(section) => PickerAction::SetModelField {
                        provider_id: result.provider_id,
                        section,
                    },
                    None => PickerAction::SetModelDefault {
                        provider_id: result.provider_id,
                    },
                };
                app.overlay = Some(Overlay::Picker(PickerOverlay {
                    title: format!("Select model for {}", provider_name),
                    items: models,
                    selected: 0,
                    action,
                }));
                app.focus = Focus::Overlay;
            }
            Err(err) => {
                error!("Model fetch failed for provider={provider_name}: {err}");
                app.overlay = None;
                app.focus = Focus::Content;
                app.show_toast(Toast::error(format!("Failed to fetch models: {err}")));
            }
        }
    }
}

fn delete_provider(app: &mut App) {
    if let Some((id, _)) = app.settings.providers.iter().nth(app.content_idx) {
        let id = id.clone();
        app.overlay = Some(Overlay::Confirm(ConfirmOverlay {
            title: "Delete Provider".into(),
            message: format!("Delete provider \"{id}\"?"),
            kind: ConfirmKind::YesNo {
                on_yes: ConfirmAction::DeleteProvider(id),
            },
        }));
        app.focus = Focus::Overlay;
    }
}

fn oauth_provider(app: &mut App) {
    if let Some((id, cfg)) = app.settings.providers.iter().nth(app.content_idx) {
        let id = id.clone();
        if cfg.resolve_type(&id).needs_api_key() {
            app.show_toast(Toast::info(
                "This provider uses API key authentication, not OAuth",
            ));
            return;
        }
        if cfg.resolve_type(&id) == ProviderType::Copilot
            && cfg
                .copilot
                .as_ref()
                .is_some_and(|c| c.oauth_app == "opencode")
        {
            app.show_toast(Toast::info(
                "OpenCode Zen uses direct GitHub token authentication",
            ));
            return;
        }
        start_oauth_flow(app, &id);
    }
}

fn save_settings_with_feedback(app: &mut App) -> bool {
    match save_settings(app) {
        Ok(SaveOutcome::Synced(path)) => {
            app.dirty = false;
            app.show_toast(Toast::success(format!(
                "Configuration saved; Claude Code synced: {}",
                path.display()
            )));
            true
        }
        Ok(SaveOutcome::ConfigSavedSyncFailed(err)) => {
            app.dirty = false;
            app.show_toast(Toast::warning(format!(
                "Config saved; Claude Code sync failed: {err}"
            )));
            false
        }
        Err(err) => {
            app.show_toast(Toast::error(format!("Save failed: {err}")));
            false
        }
    }
}

enum SaveOutcome {
    Synced(PathBuf),
    ConfigSavedSyncFailed(String),
}

fn save_settings(app: &App) -> Result<SaveOutcome, String> {
    let path = Settings::config_file_path().unwrap_or_else(|| PathBuf::from("config.toml"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create config directory: {e}"))?;
    }
    std::fs::write(&path, app.settings.to_toml())
        .map_err(|e| format!("failed to save config: {e}"))?;
    match sync_claude_code_settings(&app.settings) {
        Ok(path) => Ok(SaveOutcome::Synced(path)),
        Err(err) => Ok(SaveOutcome::ConfigSavedSyncFailed(err)),
    }
}

fn sync_claude_code_settings(settings: &Settings) -> Result<PathBuf, String> {
    let path = claude_code_settings_path()?;
    let mut value = if path.exists() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("failed to read Claude Code settings: {e}"))?;
        if content.trim().is_empty() {
            Value::Object(Map::new())
        } else {
            serde_json::from_str(&content)
                .map_err(|e| format!("failed to parse Claude Code settings JSON: {e}"))?
        }
    } else {
        Value::Object(Map::new())
    };

    apply_claude_code_env(&mut value, settings);

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create Claude Code config directory: {e}"))?;
    }
    let content = serde_json::to_string_pretty(&value)
        .map_err(|e| format!("failed to serialize Claude Code settings: {e}"))?;
    std::fs::write(&path, format!("{content}\n"))
        .map_err(|e| format!("failed to write Claude Code settings: {e}"))?;
    Ok(path)
}

fn claude_code_settings_path() -> Result<PathBuf, String> {
    let dir = claude_code_config_dir()?;
    let settings = dir.join("settings.json");
    if settings.exists() {
        return Ok(settings);
    }
    let legacy = dir.join("claude.json");
    if legacy.exists() {
        return Ok(legacy);
    }
    Ok(settings)
}

fn claude_code_config_dir() -> Result<PathBuf, String> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.as_os_str().is_empty() && !dir.to_string_lossy().trim().is_empty() {
            return Ok(dir);
        }
    }
    home_dir()
        .map(|home| home.join(".claude"))
        .ok_or_else(|| "could not determine home directory for Claude Code settings".to_string())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
}

fn apply_claude_code_env(value: &mut Value, settings: &Settings) {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    let root = value.as_object_mut().expect("value is an object");
    let env_value = root
        .entry("env".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !env_value.is_object() {
        *env_value = Value::Object(Map::new());
    }
    let env = env_value.as_object_mut().expect("env is an object");

    set_env(env, "ANTHROPIC_BASE_URL", &claude_code_base_url(settings));
    set_env(env, "ANTHROPIC_API_KEY", &settings.server.auth_token);
    env.remove("ANTHROPIC_AUTH_TOKEN");
    set_env(env, "ANTHROPIC_MODEL", &settings.model.default);
    set_optional_env(
        env,
        "ANTHROPIC_REASONING_MODEL",
        settings.model.reasoning.as_deref(),
    );
    set_optional_env(
        env,
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        settings.model.haiku.as_deref(),
    );
    set_optional_env(
        env,
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        settings.model.sonnet.as_deref(),
    );
    set_optional_env(
        env,
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        settings.model.opus.as_deref(),
    );
    env.remove("ANTHROPIC_SMALL_FAST_MODEL");
}

fn claude_code_base_url(settings: &Settings) -> String {
    let host = match settings.server.host.as_str() {
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        host if host.contains(':') && !host.starts_with('[') => format!("[{host}]"),
        host => host.to_string(),
    };
    format!("http://{}:{}", host, settings.server.port)
}

fn set_optional_env(env: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) => {
            env.insert(key.to_string(), Value::String(value.to_string()));
        }
        None => {
            env.remove(key);
        }
    }
}

fn set_env(env: &mut Map<String, Value>, key: &str, value: &str) {
    env.insert(key.to_string(), Value::String(value.trim().to_string()));
}

fn set_model_field(app: &mut App, section: &EditableSection, value: &str) {
    let v = value.to_string();
    match section {
        EditableSection::ModelDefault => app.settings.model.default = v,
        EditableSection::ModelReasoning => {
            app.settings.model.reasoning = if v.is_empty() { None } else { Some(v) }
        }
        EditableSection::ModelOpus => {
            app.settings.model.opus = if v.is_empty() { None } else { Some(v) }
        }
        EditableSection::ModelSonnet => {
            app.settings.model.sonnet = if v.is_empty() { None } else { Some(v) }
        }
        EditableSection::ModelHaiku => {
            app.settings.model.haiku = if v.is_empty() { None } else { Some(v) }
        }
        _ => {}
    }
}

/// Kick off a background thread to fetch live metrics (non-blocking).
fn fetch_live_metrics(app: &mut App) {
    // Don't spawn a new fetch if one is already in-flight
    if app.metrics_rx.is_some() {
        return;
    }

    let host = app.settings.server.host.clone();
    let port = app.settings.server.port;
    let admin_token = app.settings.admin_auth_token().to_string();
    let url = format!("http://{host}:{port}/admin/metrics");

    let (tx, rx) = std::sync::mpsc::channel();
    app.metrics_rx = Some(rx);

    std::thread::spawn(move || {
        let client = reqwest::blocking::Client::new();
        let mut req = client.get(&url).timeout(Duration::from_secs(2));
        if !admin_token.is_empty() {
            req = req.header("Authorization", format!("Bearer {admin_token}"));
        }
        let result = req
            .send()
            .ok()
            .and_then(|r| r.json::<serde_json::Value>().ok());
        let _ = tx.send(result);
    });
}

/// Poll for completed metrics fetch results (called every tick).
fn poll_metrics(app: &mut App) {
    let data = if let Some(ref rx) = app.metrics_rx {
        match rx.try_recv() {
            Ok(data) => {
                // Got result, clear the channel
                Some(data)
            }
            Err(TryRecvError::Empty) => return, // still in-flight
            Err(TryRecvError::Disconnected) => Some(None), // thread died
        }
    } else {
        return;
    };
    app.metrics_rx = None;

    let Some(Some(data)) = data else { return };

    let mut live = LiveMetrics {
        requests_total: data
            .get("requests_total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        errors_total: data
            .get("errors_total")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        avg_latency_ms: data
            .get("avg_latency_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        models: Vec::new(),
        stored: None,
    };

    if let Some(models) = data.get("models").and_then(|v| v.as_object()) {
        let mut model_list: Vec<(String, LiveModelMetrics)> = models
            .iter()
            .map(|(name, v)| {
                let m = LiveModelMetrics {
                    requests: v.get("requests").and_then(|x| x.as_u64()).unwrap_or(0),
                    input_tokens: v.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                    output_tokens: v.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                    cache_creation_input_tokens: v
                        .get("cache_creation_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                    cache_read_input_tokens: v
                        .get("cache_read_input_tokens")
                        .and_then(|x| x.as_u64())
                        .unwrap_or(0),
                };
                (name.clone(), m)
            })
            .collect();
        model_list.sort_by_key(|a| std::cmp::Reverse(a.1.total_tokens()));
        live.models = model_list;
    }

    // Parse stored (all-time) metrics
    if let Some(stored) = data.get("stored") {
        let mut stored_metrics = StoredMetrics {
            requests_total: stored
                .get("requests_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            errors_total: stored
                .get("errors_total")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            avg_latency_ms: stored
                .get("avg_latency_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            models: Vec::new(),
        };
        if let Some(models) = stored.get("models").and_then(|v| v.as_object()) {
            let mut model_list: Vec<(String, LiveModelMetrics)> = models
                .iter()
                .map(|(name, v)| {
                    let m = LiveModelMetrics {
                        requests: v.get("requests").and_then(|x| x.as_u64()).unwrap_or(0),
                        input_tokens: v.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                        output_tokens: v.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
                        cache_creation_input_tokens: v
                            .get("cache_creation_input_tokens")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0),
                        cache_read_input_tokens: v
                            .get("cache_read_input_tokens")
                            .and_then(|x| x.as_u64())
                            .unwrap_or(0),
                    };
                    (name.clone(), m)
                })
                .collect();
            model_list.sort_by_key(|a| std::cmp::Reverse(a.1.total_tokens()));
            stored_metrics.models = model_list;
        }
        live.stored = Some(stored_metrics);
    }

    app.live_metrics = Some(live);
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_proxy_config::settings::{ModelConfig, ServerConfig};
    use serde_json::json;

    #[test]
    fn claude_code_env_sync_sets_proxy_and_model_keys() {
        let mut settings = Settings {
            model: ModelConfig {
                default: "copilot/claude-sonnet-4".to_string(),
                reasoning: Some("copilot/deepseek-v4-flash".to_string()),
                opus: Some("copilot/claude-opus-4".to_string()),
                sonnet: Some("openai/gpt-5".to_string()),
                haiku: Some("openai/gpt-5-mini".to_string()),
            },
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                port: 18082,
                auth_token: "proxy-token".to_string(),
            },
            ..Settings::default()
        };
        settings.infer_provider_types();

        let mut value = json!({
            "theme": "dark",
            "env": {
                "KEEP_ME": "yes",
                "ANTHROPIC_AUTH_TOKEN": "legacy-token",
                "ANTHROPIC_SMALL_FAST_MODEL": "legacy"
            }
        });

        apply_claude_code_env(&mut value, &settings);

        let env = value["env"].as_object().expect("env object");
        assert_eq!(
            env.get("ANTHROPIC_BASE_URL").and_then(|v| v.as_str()),
            Some("http://127.0.0.1:18082")
        );
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").and_then(|v| v.as_str()),
            Some("proxy-token")
        );
        assert_eq!(
            env.get("ANTHROPIC_MODEL").and_then(|v| v.as_str()),
            Some("copilot/claude-sonnet-4")
        );
        assert_eq!(
            env.get("ANTHROPIC_REASONING_MODEL")
                .and_then(|v| v.as_str()),
            Some("copilot/deepseek-v4-flash")
        );
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_OPUS_MODEL")
                .and_then(|v| v.as_str()),
            Some("copilot/claude-opus-4")
        );
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_SONNET_MODEL")
                .and_then(|v| v.as_str()),
            Some("openai/gpt-5")
        );
        assert_eq!(
            env.get("ANTHROPIC_DEFAULT_HAIKU_MODEL")
                .and_then(|v| v.as_str()),
            Some("openai/gpt-5-mini")
        );
        assert_eq!(env.get("KEEP_ME").and_then(|v| v.as_str()), Some("yes"));
        assert!(!env.contains_key("ANTHROPIC_AUTH_TOKEN"));
        assert!(!env.contains_key("ANTHROPIC_SMALL_FAST_MODEL"));
        assert_eq!(value["theme"].as_str(), Some("dark"));
    }

    #[test]
    fn claude_code_env_sync_removes_empty_optional_model_keys() {
        let settings = Settings {
            model: ModelConfig {
                default: "openai/gpt-5".to_string(),
                reasoning: Some("   ".to_string()),
                opus: None,
                sonnet: Some("   ".to_string()),
                haiku: None,
            },
            ..Settings::default()
        };
        let mut value = json!({
            "env": {
                "ANTHROPIC_DEFAULT_OPUS_MODEL": "old-opus",
                "ANTHROPIC_DEFAULT_SONNET_MODEL": "old-sonnet",
                "ANTHROPIC_DEFAULT_HAIKU_MODEL": "old-haiku",
                "ANTHROPIC_REASONING_MODEL": "old-reasoning"
            }
        });

        apply_claude_code_env(&mut value, &settings);

        let env = value["env"].as_object().expect("env object");
        assert_eq!(
            env.get("ANTHROPIC_MODEL").and_then(|v| v.as_str()),
            Some("openai/gpt-5")
        );
        assert!(!env.contains_key("ANTHROPIC_REASONING_MODEL"));
        assert!(!env.contains_key("ANTHROPIC_DEFAULT_OPUS_MODEL"));
        assert!(!env.contains_key("ANTHROPIC_DEFAULT_SONNET_MODEL"));
        assert!(!env.contains_key("ANTHROPIC_DEFAULT_HAIKU_MODEL"));
    }

    #[test]
    fn ctrl_s_applies_pending_model_input_before_save() {
        let mut app = App::new(Settings::default());
        app.overlay = Some(Overlay::Input(
            InputOverlay::new(
                "Default Model",
                "Model",
                InputAction::EditSetting {
                    section: EditableSection::ModelDefault,
                },
            )
            .with_value("openai/gpt-5"),
        ));
        app.focus = Focus::Overlay;

        assert!(apply_pending_input(&mut app));

        assert_eq!(app.settings.model.default, "openai/gpt-5");
        assert!(app.overlay.is_none());
        assert_eq!(app.focus, Focus::Content);
        assert!(app.dirty);
    }

    #[test]
    fn pending_input_failure_keeps_settings_unsaved() {
        let mut app = App::new(Settings::default());
        let original_port = app.settings.server.port;
        app.overlay = Some(Overlay::Input(
            InputOverlay::new(
                "Port",
                "Port",
                InputAction::EditSetting {
                    section: EditableSection::ServerPort,
                },
            )
            .with_value("not-a-port"),
        ));

        assert!(!apply_pending_input(&mut app));

        assert_eq!(app.settings.server.port, original_port);
        assert!(!app.dirty);
    }
}
