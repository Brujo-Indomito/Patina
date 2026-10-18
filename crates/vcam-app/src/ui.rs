//! The settings window and tray icon.
//!
//! Closing the window hides it to the tray; the virtual camera keeps running.
//! The tray menu reopens the window or quits (which removes the camera).
//! Every change is saved immediately and applied to the running camera.

use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use eframe::egui::{self, Color32, RichText, TextureHandle, TextureOptions, ViewportCommand};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Media::MediaFoundation::{MFShutdown, MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};

use crate::controller::Controller;
use crate::devices;
use crate::publisher::{PREVIEW_HEIGHT, PREVIEW_WIDTH};
use crate::settings::{self, EffectKind, ModelChoice, Settings};
use crate::{icon, FRIENDLY_NAME};

/// Opens the window (hidden in the tray if `start_hidden`) and runs until Quit.
pub fn run(start_hidden: bool) -> Result<(), String> {
    let icon_size = 64;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(FRIENDLY_NAME)
            .with_inner_size([560.0, 760.0])
            .with_min_inner_size([420.0, 480.0])
            .with_visible(!start_hidden)
            .with_icon(egui::IconData { rgba: icon::rgba(icon_size), width: icon_size, height: icon_size }),
        ..Default::default()
    };
    eframe::run_native(FRIENDLY_NAME, options, Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))))
        .map_err(|e| e.to_string())
}

enum TrayAction {
    Show,
    Quit,
}

struct App {
    settings: Settings,
    saved: Settings,
    save_error: Option<String>,
    controller: Option<Controller>,
    cameras: Vec<String>,
    actions: Receiver<TrayAction>,
    _tray: Option<TrayIcon>,
    quitting: bool,
    preview: Option<TextureHandle>,
    preview_sequence: u64,
    mf_started: bool,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        // Webcam enumeration on this thread needs COM and Media Foundation.
        let mf_started = unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok()
        };

        let mut settings = Settings::load();
        settings.start_with_windows = settings::start_with_windows_enabled();
        let (sender, actions) = channel();
        let tray = create_tray(ctx, sender);
        let controller = Controller::start(settings.to_config());

        Self {
            saved: settings.clone(),
            settings,
            save_error: None,
            controller: Some(controller),
            cameras: list_cameras(),
            actions,
            _tray: tray,
            quitting: false,
            preview: None,
            preview_sequence: 0,
            mf_started,
        }
    }

    fn controller(&self) -> &Controller {
        self.controller.as_ref().expect("controller lives until drop")
    }

    /// Saves and applies settings that changed during this frame.
    fn apply_changes(&mut self) {
        if self.settings == self.saved {
            return;
        }
        if self.settings.start_with_windows != self.saved.start_with_windows {
            let result = std::env::current_exe()
                .map_err(|e| e.to_string())
                .and_then(|exe| settings::set_start_with_windows(self.settings.start_with_windows, &exe));
            if let Err(err) = result {
                self.settings.start_with_windows = self.saved.start_with_windows;
                self.save_error = Some(err);
            }
        }
        self.save_error = self.settings.save().err().or(self.save_error.take());
        self.controller().control().set_config(self.settings.to_config());
        self.saved = self.settings.clone();
    }

    fn update_preview(&mut self, ctx: &egui::Context) {
        let control = self.controller().control();
        control.want_preview();
        if let Some(frame) = control.take_preview(self.preview_sequence) {
            self.preview_sequence = frame.sequence;
            let image = egui::ColorImage::from_rgba_unmultiplied([PREVIEW_WIDTH, PREVIEW_HEIGHT], &frame.rgba);
            match &mut self.preview {
                Some(texture) => texture.set(image, TextureOptions::LINEAR),
                None => self.preview = Some(ctx.load_texture("preview", image, TextureOptions::LINEAR)),
            }
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Stop the publisher and remove the virtual camera before Media Foundation shuts down.
        self.controller = None;
        if self.mf_started {
            unsafe { let _ = MFShutdown(); }
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(action) = self.actions.try_recv() {
            match action {
                TrayAction::Show => {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
                TrayAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
        // The close button hides to the tray; only Quit exits.
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.settings.show_preview {
            self.update_preview(&ctx);
            ctx.request_repaint_after(Duration::from_millis(33));
        } else {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        let status = self.controller().control().status();

        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                status_header(ui, &status);
                ui.add_space(8.0);

                ui.checkbox(&mut self.settings.show_preview, "Show preview");
                if self.settings.show_preview {
                    let width = ui.available_width().min(PREVIEW_WIDTH as f32);
                    let size = egui::vec2(width, width * PREVIEW_HEIGHT as f32 / PREVIEW_WIDTH as f32);
                    match &self.preview {
                        Some(texture) => {
                            ui.add(egui::Image::new((texture.id(), size)).corner_radius(6.0));
                        }
                        None => {
                            ui.allocate_ui(size, |ui| ui.centered_and_justified(|ui| ui.weak("Starting the webcam…")));
                        }
                    }
                }
                ui.add_space(12.0);

                ui.heading("Settings");
                ui.add_space(4.0);
                self.settings_grid(ui);
                ui.add_space(12.0);

                ui.heading("Status");
                ui.add_space(4.0);
                status_grid(ui, &status);
                ui.add_space(8.0);

                egui::CollapsingHeader::new("Activity log").show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(160.0).stick_to_bottom(true).show(ui, |ui| {
                        for line in &status.log {
                            ui.label(RichText::new(line).monospace().small());
                        }
                    });
                });

                ui.add_space(8.0);
                match &self.save_error {
                    Some(err) => ui.colored_label(Color32::from_rgb(220, 80, 80), err),
                    None => ui.weak(format!("Settings are saved to {}", Settings::path().display())),
                };
            });
        });

        self.apply_changes();
    }
}

impl App {
    fn settings_grid(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings").num_columns(2).spacing([16.0, 10.0]).show(ui, |ui| {
            ui.label("Webcam");
            ui.horizontal(|ui| {
                let selected = self.settings.camera.clone().unwrap_or_else(|| "Automatic".into());
                egui::ComboBox::from_id_salt("camera").selected_text(selected).width(260.0).show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.settings.camera, None, "Automatic");
                    for name in &self.cameras {
                        ui.selectable_value(&mut self.settings.camera, Some(name.clone()), name);
                    }
                });
                if ui.button("Refresh").on_hover_text("Refresh the webcam list").clicked() {
                    self.cameras = list_cameras();
                }
            });
            ui.end_row();

            ui.label("Background");
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.settings.effect, EffectKind::None, "None");
                ui.radio_value(&mut self.settings.effect, EffectKind::Blur, "Blur");
                ui.radio_value(&mut self.settings.effect, EffectKind::Background, "Image");
            });
            ui.end_row();

            ui.label("Blur strength");
            ui.add_enabled(
                self.settings.effect == EffectKind::Blur,
                egui::Slider::new(&mut self.settings.blur_strength, 1..=20),
            );
            ui.end_row();

            ui.label("Background image");
            ui.add_enabled_ui(self.settings.effect == EffectKind::Background, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Choose…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().add_filter("Images", &["jpg", "jpeg", "png"]).pick_file() {
                            self.settings.background_image = Some(path);
                        }
                    }
                    match &self.settings.background_image {
                        Some(path) => {
                            let name = path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
                            ui.label(name).on_hover_text(path.display().to_string());
                        }
                        None => {
                            ui.weak("No image chosen");
                        }
                    }
                });
            });
            ui.end_row();

            ui.label("Segmentation model");
            let current = self.settings.model;
            egui::ComboBox::from_id_salt("model").selected_text(current.label()).width(260.0).show_ui(ui, |ui| {
                for choice in ModelChoice::ALL {
                    let available = choice.path(self.settings.use_gpu).is_some();
                    let label = if available { choice.label().to_string() } else { format!("{} (not found)", choice.label()) };
                    ui.add_enabled_ui(available, |ui| ui.selectable_value(&mut self.settings.model, choice, label));
                }
            });
            ui.end_row();

            ui.label("Acceleration");
            ui.checkbox(&mut self.settings.use_gpu, "Use the GPU (DirectML)");
            ui.end_row();

            ui.label("Startup");
            ui.checkbox(&mut self.settings.start_with_windows, "Start with Windows (in the tray)");
            ui.end_row();
        });
    }
}

fn status_header(ui: &mut egui::Ui, status: &crate::publisher::Status) {
    let (live, text) = match status.client {
        Some(format) => (true, format!("Live: a video app is receiving {format}")),
        None => (false, format!("Idle: select \"{FRIENDLY_NAME}\" in Zoom, Teams or the Camera app")),
    };
    ui.horizontal(|ui| {
        // Drawn rather than typed: the built-in font has no circle glyphs.
        let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
        let (center, radius) = (rect.center(), 5.0);
        if live {
            ui.painter().circle_filled(center, radius, Color32::from_rgb(60, 180, 90));
        } else {
            ui.painter().circle_stroke(center, radius, egui::Stroke::new(1.5, ui.visuals().weak_text_color()));
        }
        ui.label(RichText::new(text).strong());
    });
    if let Some(problem) = &status.problem {
        ui.colored_label(Color32::from_rgb(220, 80, 80), problem);
    }
}

fn status_grid(ui: &mut egui::Ui, status: &crate::publisher::Status) {
    egui::Grid::new("status").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
        ui.label("Webcam");
        ui.label(status.camera.as_deref().unwrap_or("closed (opens when a video app or the preview needs it)"));
        ui.end_row();

        if status.camera.is_some() && status.fps > 0.0 {
            ui.label("Frame rate");
            ui.label(format!("{:.1} fps ({:.1} new images per second)", status.fps, status.new_image_fps));
            ui.end_row();
        }
        if let Some(segmentation) = &status.segmentation {
            ui.label("Segmentation");
            ui.label(format!("{segmentation}, {:.0}/s at {:.1} ms", status.segmentation_rate, status.segmentation_ms));
            ui.end_row();
        }
        if let Some((prepare, wait, composite)) = status.timings {
            ui.label("Per frame");
            ui.label(format!("prepare {prepare:.1} ms, wait {wait:.1} ms, composite {composite:.1} ms"));
            ui.end_row();
        }
        if let Some((low, high)) = status.coverage {
            ui.label("Person");
            ui.label(format!("{:.0}-{:.0}% of the frame", low * 100.0, high * 100.0));
            ui.end_row();
        }
    });
}

/// Webcam names, excluding our own virtual camera.
fn list_cameras() -> Vec<String> {
    devices::video_devices()
        .map(|list| list.into_iter().map(|d| d.name).filter(|n| !n.starts_with(devices::OWN_CAMERA_PREFIX)).collect())
        .unwrap_or_default()
}

fn create_tray(ctx: &egui::Context, sender: std::sync::mpsc::Sender<TrayAction>) -> Option<TrayIcon> {
    let menu = Menu::new();
    let open = MenuItem::with_id("open", format!("Open {FRIENDLY_NAME}"), true, None);
    let quit = MenuItem::with_id("quit", "Quit (removes the camera)", true, None);
    menu.append(&open).ok()?;
    menu.append(&PredefinedMenuItem::separator()).ok()?;
    menu.append(&quit).ok()?;

    let icon = Icon::from_rgba(icon::rgba(32), 32, 32).ok()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(FRIENDLY_NAME)
        .with_icon(icon)
        .build()
        .ok()?;

    // Handlers run on the UI thread; wake egui so `logic` handles the action
    // even while the window is hidden.
    let (menu_ctx, menu_sender) = (ctx.clone(), sender.clone());
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let action = match event.id.0.as_str() {
            "open" => TrayAction::Show,
            "quit" => TrayAction::Quit,
            _ => return,
        };
        let _ = menu_sender.send(action);
        menu_ctx.request_repaint();
    }));
    let icon_ctx = ctx.clone();
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        let open = match event {
            TrayIconEvent::DoubleClick { .. } => true,
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } => true,
            _ => false,
        };
        if open {
            let _ = sender.send(TrayAction::Show);
            icon_ctx.request_repaint();
        }
    }));
    Some(tray)
}
