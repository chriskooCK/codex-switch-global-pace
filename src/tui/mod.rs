pub mod app;
pub mod keymap;
pub mod menu;
mod meter;
pub mod popup;
mod switch;
pub mod ui;

use anyhow::Result;

pub async fn run_tui() -> Result<()> {
    app::run().await
}
