use eframe::egui;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

fn main() -> eframe::Result<()> {
    // Setup options: Undecorated, Top of screen, Fixed height
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_always_on_top()
            .with_inner_size([1920.0, 40.0])
            .with_position(egui::pos2(0.0, 0.0)),
        ..Default::default()
    };

    eframe::run_native(
        "DeeMenu",
        options,
        Box::new(|cc| Ok(Box::new(DeeMenu::new(cc)))),
    )
}

#[derive(PartialEq)]
enum AppMode {
    Search,
    SudoPassword,
}

struct DeeMenu {
    // --- Logic State ---
    all_executables: Vec<String>,
    filtered_executables: Vec<String>,
    search_query: String,
    password_query: String,
    selected_index: usize,
    mode: AppMode,
    pending_sudo_command: String,

    // --- UI State ---
    startup_counter: u8,
}

impl DeeMenu {
    fn new(cc: &eframe::CreationContext) -> Self {
        // Visual Style
        let mut visuals = egui::Visuals::dark();
        visuals.override_text_color = Some(egui::Color32::WHITE);
        visuals.panel_fill = egui::Color32::from_rgb(35, 36, 41);
        cc.egui_ctx.set_visuals(visuals);

        let mut style = (*cc.egui_ctx.style()).clone();
        style.text_styles.insert(
            egui::TextStyle::Body,
            egui::FontId::new(14.0, egui::FontFamily::Monospace),
        );
        cc.egui_ctx.set_style(style);

        let mut app = Self {
            all_executables: Vec::new(),
            filtered_executables: Vec::new(),
            search_query: String::new(),
            password_query: String::new(),
            selected_index: 0,
            mode: AppMode::Search,
            pending_sudo_command: String::new(),
            startup_counter: 0,
        };

        app.scan_path();
        app
    }

    /// Scans PATH + Standard Linux Directories (Permissive Mode)
    fn scan_path(&mut self) {
        let mut binaries = HashSet::new();

        // 1. Get paths from Environment
        let path_var = env::var("PATH").unwrap_or_default();
        let mut paths_to_scan: Vec<String> = env::split_paths(&path_var)
            .map(|p| p.to_string_lossy().to_string())
            .collect();

        // 2. Force add standard directories (to catch /usr/bin if PATH is minimal)
        let fallback_paths = [
            "/usr/bin",
            "/usr/local/bin",
            "/bin",
            "/snap/bin",
            "/var/lib/flatpak/exports/bin",
            "/sbin",
            "/usr/sbin"
        ];

        for fallback in fallback_paths {
            let p = fallback.to_string();
            if !paths_to_scan.contains(&p) {
                paths_to_scan.push(p);
            }
        }

        for path_str in &paths_to_scan {
            let path = Path::new(path_str);

            if !path.exists() { continue; }

            if let Ok(entries) = fs::read_dir(path) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();

                    // Skip hidden files
                    if name.starts_with('.') { continue; }

                    // PERMISSIVE CHECK:
                    // If it is in a bin folder and not a directory, assume it is executable.
                    // This fixes issues where symlinks (like firefox -> ../lib/firefox/firefox.sh)
                    // were being ignored by strict metadata checks.
                    if let Ok(file_type) = entry.file_type() {
                        if !file_type.is_dir() {
                             binaries.insert(name);
                        }
                    }
                }
            }
        }

        self.all_executables = binaries.into_iter().collect();
        self.all_executables.sort();
        self.update_filter();
    }

    fn update_filter(&mut self) {
        let query = self.search_query.trim().to_lowercase();

        // Handle sudo prefix logic for filtering
        let clean_query = if query.starts_with("sudo ") {
            query.strip_prefix("sudo ").unwrap_or("").to_string()
        } else {
            query.clone()
        };

        if clean_query.is_empty() {
            self.filtered_executables = self.all_executables.iter().take(50).cloned().collect();
        } else {
            self.filtered_executables = self.all_executables
                .iter()
                .filter(|name| name.to_lowercase().contains(&clean_query))
                .take(50)
                .cloned()
                .collect();
        }

        // Safety bounds
        if self.filtered_executables.is_empty() {
            self.selected_index = 0;
        } else if self.selected_index >= self.filtered_executables.len() {
            self.selected_index = self.filtered_executables.len() - 1;
        }
    }

    fn attempt_run(&mut self) -> bool {
        match self.mode {
            AppMode::Search => {
                let raw_cmd = self.search_query.trim();

                // 1. Detect Sudo Request
                if raw_cmd.starts_with("sudo ") {
                    let actual_cmd = raw_cmd.strip_prefix("sudo ").unwrap().trim();
                    if !actual_cmd.is_empty() {
                        self.pending_sudo_command = actual_cmd.to_string();
                        self.mode = AppMode::SudoPassword;
                        self.selected_index = 0;
                        return false; // Don't close, wait for password
                    }
                    return false;
                }

                // 2. Determine Command
                // If user typed arguments (spaces) OR no match found, use raw input.
                // Otherwise use the selected suggestion.
                let cmd_to_run = if !self.filtered_executables.is_empty() {
                    if raw_cmd.contains(' ') {
                        raw_cmd.to_string()
                    } else {
                        self.filtered_executables[self.selected_index].clone()
                    }
                } else {
                    raw_cmd.to_string()
                };

                if !cmd_to_run.is_empty() {
                    self.spawn_process(&cmd_to_run, false, None);
                    return true;
                }
            }
            AppMode::SudoPassword => {
                if !self.password_query.is_empty() {
                    self.spawn_process(&self.pending_sudo_command, true, Some(self.password_query.clone()));
                    return true;
                }
            }
        }
        false
    }

    fn spawn_process(&self, cmd_str: &str, is_sudo: bool, password: Option<String>) {
        let cmd_str = cmd_str.to_string();

        thread::spawn(move || {
            if is_sudo {
                // Sudo pipe execution
                let parts: Vec<&str> = cmd_str.split_whitespace().collect();
                if parts.is_empty() { return; }

                let mut child = Command::new("sudo")
                    .arg("-S") // Read stdin
                    .arg("-k") // Ignore cache
                    .arg("--")
                    .args(parts)
                    .stdin(Stdio::piped())
                    .spawn()
                    .expect("Failed to spawn sudo");

                if let Some(mut stdin) = child.stdin.take() {
                    if let Some(pw) = password {
                        let _ = stdin.write_all(pw.as_bytes());
                    }
                }
            } else {
                // Normal execution
                let parts: Vec<&str> = cmd_str.split_whitespace().collect();
                if let Some((cmd, args)) = parts.split_first() {
                    let _ = Command::new(cmd)
                        .args(args)
                        .spawn();
                }
            }
        });
    }
}

impl eframe::App for DeeMenu {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // --- Startup Positioning Fix ---
        if self.startup_counter < 3 {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(0.0, 0.0)));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            self.startup_counter += 1;
            ctx.request_repaint();
        }

        // --- Input ---
        let esc_pressed = ctx.input(|i| i.key_pressed(egui::Key::Escape));
        let enter_pressed = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let tab_pressed = ctx.input(|i| i.key_pressed(egui::Key::Tab));
        let arrow_right = ctx.input(|i| i.key_pressed(egui::Key::ArrowRight));
        let arrow_left = ctx.input(|i| i.key_pressed(egui::Key::ArrowLeft));

        if esc_pressed {
            if self.mode == AppMode::SudoPassword {
                self.mode = AppMode::Search;
                self.password_query.clear();
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        // Navigation (Search Mode Only)
        if self.mode == AppMode::Search && !self.filtered_executables.is_empty() {
            if arrow_right || tab_pressed {
                self.selected_index = (self.selected_index + 1) % self.filtered_executables.len();
            }
            if arrow_left {
                if self.selected_index == 0 {
                    self.selected_index = self.filtered_executables.len() - 1;
                } else {
                    self.selected_index -= 1;
                }
            }
        }

        let mut should_close = false;

        // --- UI Rendering ---
        let panel_color = match self.mode {
            AppMode::Search => egui::Color32::from_rgb(35, 36, 41),
            AppMode::SudoPassword => egui::Color32::from_rgb(60, 20, 20),
        };

        egui::CentralPanel::default().frame(egui::Frame::none().fill(panel_color)).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.style_mut().spacing.item_spacing = egui::vec2(8.0, 0.0);
                ui.add_space(5.0);

                match self.mode {
                    // SEARCH MODE
                    AppMode::Search => {
                        let font_id = egui::FontId::new(14.0, egui::FontFamily::Monospace);

                        let text_width = ui.fonts(|f| {
                            f.layout_no_wrap(self.search_query.clone(), font_id, egui::Color32::WHITE).rect.width()
                        });
                        let box_width = (text_width + 20.0).max(100.0);

                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.search_query)
                                .hint_text("Run...")
                                .frame(false)
                                .desired_width(box_width)
                        );

                        if self.startup_counter < 3 || !ui.memory(|m| m.has_focus(response.id)) {
                            response.request_focus();
                        }

                        if response.changed() {
                            self.selected_index = 0;
                            self.update_filter();
                        }

                        ui.label(egui::RichText::new("|").color(egui::Color32::GRAY));

                        // Store click result to process outside loop
                        let mut clicked_index = None;

                        egui::ScrollArea::horizontal().show(ui, |ui| {
                            for (i, name) in self.filtered_executables.iter().enumerate() {
                                let is_selected = i == self.selected_index;

                                let bg_color = if is_selected {
                                    egui::Color32::from_rgb(217, 70, 239)
                                } else {
                                    panel_color
                                };

                                let text_color = if is_selected {
                                    egui::Color32::WHITE
                                } else {
                                    egui::Color32::from_rgb(171, 178, 191)
                                };

                                let galley = ui.painter().layout_no_wrap(
                                    name.clone(),
                                    egui::FontId::new(14.0, egui::FontFamily::Monospace),
                                    text_color
                                );

                                let padding = egui::vec2(12.0, 6.0);
                                let rect_size = galley.size() + padding;
                                let (rect, resp) = ui.allocate_at_least(rect_size, egui::Sense::click());

                                ui.painter().rect_filled(rect, 2.0, bg_color);

                                let text_pos = rect.min + egui::vec2(6.0, (rect.height() - galley.size().y) / 2.0);
                                ui.painter().galley(text_pos, galley, egui::Color32::PLACEHOLDER);

                                if resp.clicked() {
                                    clicked_index = Some(i);
                                }

                                if is_selected {
                                    ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                }
                            }
                        });

                        // Handle mouse click
                        if let Some(i) = clicked_index {
                            self.selected_index = i;
                            self.search_query = self.filtered_executables[i].clone();
                            should_close = self.attempt_run();
                        }
                    }

                    // PASSWORD MODE
                    AppMode::SudoPassword => {
                        ui.label(
                            egui::RichText::new("🔒 SUDO PASSWORD:")
                                .color(egui::Color32::from_rgb(255, 100, 100))
                                .strong()
                        );

                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.password_query)
                                .password(true)
                                .frame(false)
                                .desired_width(200.0)
                        );

                        // Force focus
                        response.request_focus();
                        ui.label(egui::RichText::new(format!("for '{}'", self.pending_sudo_command)).italics());
                    }
                }
            });
        });

        // Handle Enter Key
        if enter_pressed {
            should_close = self.attempt_run();
        }

        if should_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}