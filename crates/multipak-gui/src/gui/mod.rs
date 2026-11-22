//! The egui/eframe desktop interface.

mod app;
mod dialogs;
pub mod model;
mod style;
mod views;
pub mod worker;

use std::sync::Arc;

use anyhow::Context as _;
use multipak_core::Manager;

pub use app::MultipakApp;

/// The app ID / window class, matching `multipak.desktop`.
pub const APP_ID: &str = "multipak";

fn window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../../assets/multipak-256.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(egui::IconData { rgba: img.into_raw(), width, height })
}

/// Opens the main window and runs until it is closed.
pub fn run(manager: Arc<Manager>, startup_warnings: Vec<String>) -> anyhow::Result<()> {
    let mut viewport = egui::ViewportBuilder::default()
        .with_app_id(APP_ID)
        .with_title("Multipak")
        .with_inner_size([1120.0, 740.0])
        .with_min_inner_size([760.0, 480.0]);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(Arc::new(icon));
    }
    let options = eframe::NativeOptions {
        viewport,
        // glow (OpenGL) works on the widest range of Linux GPUs, drivers, VMs and
        // remote sessions; see the README for the reasoning.
        renderer: eframe::Renderer::Glow,
        persist_window: true,
        ..Default::default()
    };
    eframe::run_native(
        APP_ID,
        options,
        Box::new(move |cc| Ok(Box::new(MultipakApp::new(cc, manager, startup_warnings)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
    .context("could not open the Multipak window (is a Wayland or X11 session running?)")
}
