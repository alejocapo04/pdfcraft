//! Headless screenshot of the real PdfCraft shell (egui_kittest + wgpu, no window needed).
//!
//! ```text
//! cargo run -p pdfcraft-ui-egui --example shot -- out.png [file.pdf] [--size 1440x900] [--scale 2] [--page 3 --panel pages …]
//! ```
//! `--file-provider-preview` installs synthetic external Recents with no backend.
//!
//! `--width N` downscales the image to N pixels wide (README screenshots). Any other
//! `--key value` pair is passed to `PdfCraftApp::set_option`
//! (the same verbs as the desktop app's command-line flags and the future control channel).

use std::time::{Duration, Instant};

use egui_kittest::Harness;
use pdfcraft_ui_egui::PdfCraftApp;

fn file_provider_preview() -> Result<pdfcraft_ui_egui::file_providers::Host, String> {
    use pdfcraft_ui_egui::file_providers::{Host, Resource};
    let host = Host::default();
    host.set_session("snapshot-provider-preview".into())?;
    let connection = host.connect("snapshot-storage".into(), "Example Storage".into(), "preview-account".into())?;
    let resources = ["Design brief.pdf", "Research notes.pdf"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| Resource { id: format!("preview-{index}"), name: name.into(), updated_at: String::new() })
        .collect();
    host.set_resources(&connection, resources)?;
    Ok(host)
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let out = args.next().ok_or("usage: shot <out.png> [file.pdf] [--option value …]")?;
    let (mut file, mut opts) = (None, Vec::new());
    let (mut size, mut scale) = (egui::vec2(1440.0, 900.0), 2.0f32);
    let mut width: Option<u32> = None;
    let mut preview_provider = false;
    while let Some(a) = args.next() {
        match a.strip_prefix("--") {
            Some("size") => {
                let v = args.next().unwrap_or_default();
                let (w, h) = v.split_once('x').ok_or("--size WxH")?;
                size = egui::vec2(w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?);
            }
            Some("scale") => scale = args.next().and_then(|v| v.parse().ok()).ok_or("bad --scale")?,
            Some("file-provider-preview") => preview_provider = true,
            Some("width") => width = Some(args.next().and_then(|v| v.parse().ok()).ok_or("bad --width")?),
            Some(k) => opts.push((k.to_string(), args.next().unwrap_or_default())),
            None => file = Some(a),
        }
    }
    let provider = if preview_provider { Some(file_provider_preview()?) } else { None };
    let mut harness = Harness::builder().with_size(size).with_pixels_per_point(scale).build_eframe(move |_cc| {
        let mut app = PdfCraftApp::new();
        if let Some(host) = &provider {
            app.file_provider = Some(pdfcraft_ui_egui::provider::Bridge::new(host.clone()));
        }
        if let Some(f) = &file {
            let bytes = std::fs::read(f).unwrap_or_default();
            let name = std::path::Path::new(f).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            if let Err(e) = app.open_bytes(&name, Some(f.clone()), bytes) {
                eprintln!("shot: {f}: {e}");
            }
        }
        for (k, v) in &opts {
            if let Err(e) = app.set_option(k, v) {
                eprintln!("shot: --{k} {v}: {e}");
            }
        }
        app
    });
    // Let fonts install, layout settle and background renders arrive.
    let start = Instant::now();
    harness.run_steps(4);
    while start.elapsed() < Duration::from_secs(20) {
        harness.run_steps(2);
        if !harness.state().render_pending() {
            harness.run_steps(3);
            if !harness.state().render_pending() {
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    let mut image = harness.render()?;
    if let Some(w) = width.filter(|w| *w < image.width()) {
        let h = (image.height() as f64 * w as f64 / image.width() as f64).round() as u32;
        image = image::imageops::resize(&image, w, h, image::imageops::FilterType::Lanczos3);
    }
    image.save(&out).map_err(|e| e.to_string())?;
    eprintln!("shot: wrote {out} ({}×{})", image.width(), image.height());
    Ok(())
}
