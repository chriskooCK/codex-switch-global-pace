pub mod app;
pub mod keymap;
pub mod menu;
mod meter;
pub mod popup;
mod switch;
pub mod ui;

use anyhow::Result;

pub async fn run_tui(file_log_writer: crate::logging::FileLogWriter) -> Result<()> {
    app::run(file_log_writer).await
}
