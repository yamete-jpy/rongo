//! rongo: a MongoDB client. `rongo --mock` opens the in-memory demo server.

// No console window beside the app on Windows (ignored elsewhere); the test
// build keeps one for its output.
#![cfg_attr(not(test), windows_subsystem = "windows")]

mod app;
mod dialogs;
mod doc_find;
mod editor;
mod export_conn;
mod highlight;
mod history_tab;
mod index_form;
mod indexes;
mod job;
mod net;
mod operations;
mod paste;
mod result_view;
mod settings;
mod shortcut;
mod sidebar;
mod stray_move;
mod tabs;
#[cfg(test)]
mod test_support;
mod theme;
mod tools;
mod undo;
mod unlock;
mod update;
mod vault_ui;

const RELEASE_ICON_RGBA: &[u8] = include_bytes!("../../../assets/icon-256.rgba");
const DEV_ICON_RGBA: &[u8] = include_bytes!("../../../assets/icon-dev-256.rgba");
const _: () = assert!(RELEASE_ICON_RGBA.len() == 256 * 256 * 4 && DEV_ICON_RGBA.len() == 256 * 256 * 4, "assets/icon-256.rgba and icon-dev-256.rgba must be 256x256 RGBA");

/// The window and Dock icon. The release scripts (bundle-macos.sh,
/// bundle-windows.sh) build with RONGO_RELEASE set, for the app bundle's
/// green leaf; any other build (cargo run, a test build) shows it in orange,
/// so it is never taken for the installed rongo. Cargo rebuilds when the
/// variable changes.
const ICON_RGBA: &[u8] = if option_env!("RONGO_RELEASE").is_some() { RELEASE_ICON_RGBA } else { DEV_ICON_RGBA };

/// Workers for the tokio runtime, given the cores available: at least four,
/// so history file I/O (a sync per saved batch, retention) on one or two
/// can't hold up finds, pages and connects on the others; at most eight.
fn io_workers(available: usize) -> usize {
    available.clamp(4, 8)
}

fn main() -> std::process::ExitCode {
    install_panic_hook();
    let mock = std::env::args().any(|a| a == "--mock");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(io_workers(std::thread::available_parallelism().map_or(4, |n| n.get())))
        .thread_name("rongo-io")
        .enable_all()
        .build()
        .expect("start tokio runtime");
    match eframe::run_native("rongo", native_options(), Box::new(move |cc| Ok(Box::new(app::RongoApp::new(cc, rt, mock))))) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Said in a window too: on Windows there is no console to read
            // it in, and the app would just vanish.
            let text = format!("rongo could not start: {e}");
            eprintln!("{text}");
            rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("rongo").set_description(text).set_buttons(rfd::MessageButtons::Ok).show();
            std::process::ExitCode::FAILURE
        }
    }
}

/// What the crash window says: the panic's message and where it happened.
fn crash_text(message: &str, location: Option<(&str, u32)>) -> String {
    match location {
        Some((file, line)) => format!("rongo stopped unexpectedly: {message}\n\nat {file}:{line}"),
        None => format!("rongo stopped unexpectedly: {message}"),
    }
}

/// The text of a panic's payload (`panic!` makes a `&str` or a `String`).
fn payload_text(payload: &(dyn std::any::Any + Send)) -> &str {
    payload.downcast_ref::<&str>().copied().or_else(|| payload.downcast_ref::<String>().map(String::as_str)).unwrap_or("(no message)")
}

/// Lets the crash window open once, for the thread that started it only.
struct CrashWindow {
    main: std::thread::ThreadId,
    shown: std::sync::atomic::AtomicBool,
}

impl CrashWindow {
    fn new(main: std::thread::ThreadId) -> Self {
        Self { main, shown: std::sync::atomic::AtomicBool::new(false) }
    }

    /// True for the first panic on the main thread: a panic on another
    /// thread is caught and handled where it was started (the engine's
    /// requests, joined helper threads), and one while the window is
    /// being shown must not open another.
    fn claim(&self) -> bool {
        std::thread::current().id() == self.main && !self.shown.swap(true, std::sync::atomic::Ordering::SeqCst)
    }
}

/// With no console on Windows a panic would end the app without a word:
/// after the usual report (stderr), the main thread's panic is also said
/// in a window, as a failed start is.
fn install_panic_hook() {
    let window = CrashWindow::new(std::thread::current().id());
    let usual = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        usual(info);
        if window.claim() {
            let text = crash_text(payload_text(info.payload()), info.location().map(|l| (l.file(), l.line())));
            rfd::MessageDialog::new().set_level(rfd::MessageLevel::Error).set_title("rongo").set_description(text).set_buttons(rfd::MessageButtons::Ok).show();
        }
    }));
}

/// The window, drawn with wgpu. eframe's wgpu setup is kept as it is: no
/// adapter selector, a high-performance adapter asked for, and Metal,
/// DirectX 12, Vulkan and GL among the backends. wgpu ranks the adapters
/// it finds (graphics card first) and keeps a software one last, so where
/// there is no graphics card (some virtual machines) Windows still starts
/// on DirectX 12's WARP ("Microsoft Basic Render Driver"). macOS draws
/// through Metal.
fn native_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
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
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    /// At least four workers, so history file I/O (syncs, retention) on one
    /// or two can't hold up finds and connects on the others; at most eight.
    #[test]
    fn io_workers_are_between_four_and_eight() {
        assert_eq!([1, 2, 4, 6, 8, 10, 16].map(super::io_workers), [4, 4, 4, 6, 8, 8, 8]);
    }

    /// The crash window says what went wrong and where.
    #[test]
    fn the_crash_text_has_the_message_and_the_place() {
        let text = super::crash_text("index out of bounds", Some(("src/tabs.rs", 42)));
        assert!(text.starts_with("rongo stopped unexpectedly"), "{text}");
        assert!(text.contains("index out of bounds") && text.contains("src/tabs.rs:42"), "{text}");
        let no_place = super::crash_text("boom", None);
        assert!(no_place.contains("boom") && !no_place.contains("at "), "{no_place}");
        // A panic's payload is a &str or a String (what `panic!` makes); anything else has no text.
        let payloads: [Box<dyn std::any::Any + Send>; 3] = [Box::new("static"), Box::new(String::from("owned")), Box::new(7u8)];
        assert_eq!(payloads.each_ref().map(|p| super::payload_text(&**p)), ["static", "owned", "(no message)"]);
    }

    /// Only the main thread's panic ends the process, and only the first
    /// is shown: a panic while showing it does not open another window.
    #[test]
    fn the_crash_window_opens_once_for_the_main_thread_only() {
        let claim = super::CrashWindow::new(std::thread::current().id());
        let other = std::thread::scope(|s| s.spawn(|| claim.claim()).join().unwrap());
        assert!(!other, "a worker thread's panic is handled elsewhere");
        assert!(claim.claim(), "the main thread's first panic");
        assert!(!claim.claim(), "a second one (the window itself panicking) is not shown");
    }

    /// wgpu chooses the adapter itself, so with no graphics card (a
    /// virtual machine) it takes the software one: there is no selector of
    /// ours, and DirectX 12, whose WARP adapter that is, is among the
    /// backends with Metal.
    #[test]
    fn wgpu_may_fall_back_to_a_software_adapter() {
        // With WGPU_BACKEND set the default setup honours it, and the backends differ.
        if std::env::var_os("WGPU_BACKEND").is_some() {
            eprintln!("skipped: WGPU_BACKEND is set");
            return;
        }
        let options = super::native_options();
        assert!(matches!(options.renderer, eframe::Renderer::Wgpu));
        let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &options.wgpu_options.wgpu_setup else { panic!("a new wgpu setup") };
        assert!(setup.native_adapter_selector.is_none());
        assert!(setup.instance_descriptor.backends.contains(eframe::wgpu::Backends::DX12 | eframe::wgpu::Backends::METAL), "{:?}", setup.instance_descriptor.backends);
    }

    /// A build that isn't a release shows the same leaf in orange on warm
    /// dark, so it is never taken for the installed (green) rongo.
    #[test]
    fn the_dev_icon_is_the_same_leaf_in_orange() {
        let (release, dev) = (super::RELEASE_ICON_RGBA, super::DEV_ICON_RGBA);
        assert!(release.chunks(4).zip(dev.chunks(4)).all(|(r, d)| r[3] == d[3]), "the same shape, pixel for pixel");
        // Red against green over the whole icon: green leans to green, orange to red.
        let warmth = |icon: &[u8]| icon.chunks(4).map(|p| (i64::from(p[0]) - i64::from(p[1])) * i64::from(p[3])).sum::<i64>();
        assert!(warmth(release) < 0 && warmth(dev) > 0, "{} {}", warmth(release), warmth(dev));
        // This test build is not a release one.
        assert!(std::ptr::eq(super::ICON_RGBA, super::DEV_ICON_RGBA) || option_env!("RONGO_RELEASE").is_some());
    }

    /// The disk image and the installer are built with RONGO_RELEASE, so
    /// what people download shows the green leaf.
    #[test]
    fn the_release_scripts_build_with_the_release_icon() {
        let scripts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
        for name in ["bundle-macos.sh", "bundle-windows.sh"] {
            let text = std::fs::read_to_string(scripts.join(name)).unwrap();
            let set = text.find("\nexport RONGO_RELEASE=1\n").unwrap_or_else(|| panic!("{name} sets RONGO_RELEASE"));
            let build = text.find("\ncargo build").unwrap_or_else(|| panic!("{name} builds"));
            assert!(set < build, "{name} sets RONGO_RELEASE before it builds");
        }
    }
}

/// Test builds count the bytes each thread has live, so a test can check
/// that something doesn't keep growing while other tests run alongside.
#[cfg(test)]
pub mod heap {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    thread_local! {
        static LIVE: Cell<isize> = const { Cell::new(0) };
    }

    struct Counting;

    fn add(n: isize) {
        // During thread teardown the slot can be gone; those bytes don't matter.
        let _ = LIVE.try_with(|l| l.set(l.get() + n));
    }

    // SAFETY: every call is passed straight to the system allocator.
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            add(l.size() as isize);
            unsafe { System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            add(-(l.size() as isize));
            unsafe { System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
            add(n as isize - l.size() as isize);
            unsafe { System.realloc(p, l, n) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: Counting = Counting;

    /// Bytes allocated and not yet freed on this thread (frees of memory
    /// allocated elsewhere count against it).
    pub fn live() -> isize {
        LIVE.with(Cell::get)
    }
}
