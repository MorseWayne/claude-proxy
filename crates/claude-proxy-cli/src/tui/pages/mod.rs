pub mod dashboard;
pub mod providers;
pub mod settings;
pub mod system;

pub use dashboard::render_dashboard;
pub use providers::render_providers;
pub use settings::render_settings_page;
pub use system::render_system_page;
