//! rongo: a MongoDB client. `rongo --mock` opens the in-memory demo server.

mod app;
mod dialogs;
mod editor;
mod highlight;
mod index_form;
mod indexes;
mod net;
mod result_view;
mod settings;
mod sidebar;
mod tabs;
mod theme;
mod tools;

const ICON_RGBA: &[u8] = include_bytes!("../../../assets/icon-256.rgba");
const _: () = assert!(ICON_RGBA.len() == 256 * 256 * 4, "assets/icon-256.rgba must be 256x256 RGBA");

fn main() -> eframe::Result {
    let mock = std::env::args().any(|a| a == "--mock");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("rongo-io")
        .enable_all()
        .build()
        .expect("start tokio runtime");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rongo")
            // The same icon as the app bundle, so the Dock does not switch
            // icons when the app starts or quits.
            .with_icon(std::sync::Arc::new(egui::IconData {
                rgba: ICON_RGBA.to_vec(),
                width: 256,
                height: 256,
            }))
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([960.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native("rongo", options, Box::new(move |cc| Ok(Box::new(app::RongoApp::new(cc, rt, mock)))))
}
