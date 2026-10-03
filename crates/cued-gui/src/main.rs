use cued_gui::{app::App, backend::Backend, find_cued};
use eframe::egui;
use std::path::PathBuf;

const USAGE: &str = "\
cued-gui: a status window for cued's jobs and runs

Usage: cued-gui [--no-auto-start]
       cued-gui --install-desktop | --uninstall-desktop

  --no-auto-start      don't start a daemon if none is running
  --install-desktop    add cued to your applications list, with its icon
  --uninstall-desktop  remove it again (`cued uninstall` does too)

The daemon is started from CUED_EXECUTABLE, else the cued beside this
binary, else the first cued on PATH.";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut auto_start = true;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--no-auto-start" => auto_start = false,
            "--install-desktop" => return launcher(true),
            "--uninstall-desktop" => return launcher(false),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("cued-gui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}").into()),
        }
    }
    let paths = cued::paths::Paths::resolve()?;
    let cued = find_cued(
        &std::env::current_exe()?,
        std::env::var_os("CUED_EXECUTABLE")
            .map(PathBuf::from)
            .as_deref(),
        &std::env::var_os("PATH").unwrap_or_default(),
    );
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([640.0, 360.0])
            .with_app_id(cued::desktop::APP_ID)
            .with_icon(cued_gui::window_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "cued",
        options,
        Box::new(move |cc| {
            cued_gui::appearance::follow(&cc.egui_ctx);
            let ctx = cc.egui_ctx.clone();
            let backend = Backend::start(paths, cued, auto_start, move || ctx.request_repaint());
            Ok(Box::new(App::new(backend)))
        }),
    )?;
    Ok(())
}

/// `--install-desktop` / `--uninstall-desktop`: the applications-list entry
/// and dock icon.
fn launcher(install: bool) -> Result<(), Box<dyn std::error::Error>> {
    let paths = cued::paths::Paths::resolve()?;
    if !install {
        match cued::desktop::remove(&paths)? {
            Some(entry) => println!("Removed {} and its icons.", entry.display()),
            None => println!("No launcher entry is installed."),
        }
        return Ok(());
    }
    let executable = std::env::current_exe()?;
    let entry = cued::desktop::install(&paths, &executable, &cued_gui::launcher_icons())?;
    println!(
        "Installed {} launching {}. cued now appears in your applications list; \
         rerun this after moving the binary.",
        entry.display(),
        executable.display()
    );
    Ok(())
}
