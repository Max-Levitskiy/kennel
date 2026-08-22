mod client;
mod config;
mod store;

use client::Client;
use eframe::egui;
use kennel_proto::ExtensionInfo;
use std::collections::HashMap;
use std::path::PathBuf;
use tray_icon::{Icon, TrayIconBuilder};

fn socket_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support/kennel/control.sock")
}

fn extensions_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support/kennel/extensions")
}

fn gui_config_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support/kennel/gui-config.json")
}

#[derive(PartialEq, Eq)]
enum Tab {
    Installed,
    Browse,
    Settings,
}

struct KennelApp {
    client: Option<Client>,
    extensions: Vec<ExtensionInfo>,
    _tray: Option<tray_icon::TrayIcon>,
    tab: Tab,
    repo_url: String,
    store_index: Option<Result<store::RepoIndex, String>>,
    install_status: HashMap<String, Result<(), String>>,
    gui_config: config::GuiConfig,
    new_repo_url: String,
    pending_enable: Option<ExtensionInfo>,
}

impl KennelApp {
    fn new() -> Self {
        let client = Client::connect(&socket_path()).ok();
        let icon = Icon::from_rgba(vec![80, 200, 120, 255], 1, 1).expect("1x1 icon"); // placeholder; replaced with a real asset once the GUI has one
        let tray = TrayIconBuilder::new().with_icon(icon).with_tooltip("kennel: starting…").build().ok();
        let gui_config = config::GuiConfig::load(&gui_config_path());
        let repo_url = gui_config.repos.first().cloned().unwrap_or_default();
        KennelApp {
            client,
            extensions: vec![],
            _tray: tray,
            tab: Tab::Installed,
            repo_url,
            store_index: None,
            install_status: HashMap::new(),
            gui_config,
            new_repo_url: String::new(),
            pending_enable: None,
        }
    }

    fn refresh(&mut self) {
        // kenneld may not have been up yet at startup, or may have been
        // restarted (LaunchAgent KeepAlive) since our last successful call --
        // either way a dead/missing client is retried here every frame rather
        // than left permanently disconnected until the GUI itself restarts.
        if self.client.is_none() {
            self.client = Client::connect(&socket_path()).ok();
        }
        let mut broken = false;
        if let Some(client) = &mut self.client {
            match client.list() {
                Ok(list) => self.extensions = list,
                Err(_) => broken = true,
            }
        }
        if broken {
            self.client = None;
        }
    }
}

impl eframe::App for KennelApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh();
        let unhealthy = self.extensions.iter().filter(|e| matches!(e.last_status, Some(kennel_proto::MonitorStatus::Unhealthy { .. } | kennel_proto::MonitorStatus::Errored { .. }))).count();
        if let Some(tray) = &self._tray {
            // `self.extensions` is empty both when nothing is wrong and when the
            // daemon can't be reached at all, so an unhealthy count computed
            // from it alone would cheerfully report "all healthy" for a kennel
            // that isn't running -- the one situation where nothing is being
            // watched at all. Connectivity is reported first, ahead of any count.
            let tooltip = if self.client.is_none() {
                "kennel: daemon not running".to_string()
            } else if unhealthy == 0 {
                "kennel: all healthy".to_string()
            } else {
                format!("kennel: {unhealthy} unhealthy")
            };
            let _ = tray.set_tooltip(Some(tooltip));
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.selectable_label(self.tab == Tab::Installed, "Installed").clicked() {
                    self.tab = Tab::Installed;
                }
                if ui.selectable_label(self.tab == Tab::Browse, "Browse").clicked() {
                    self.tab = Tab::Browse;
                }
                if ui.selectable_label(self.tab == Tab::Settings, "Settings").clicked() {
                    self.tab = Tab::Settings;
                }
            });
            ui.separator();

            match self.tab {
                Tab::Installed => {
                    ui.heading("Installed");
                    if self.client.is_none() {
                        ui.label("kenneld is not running");
                        return;
                    }
                    for ext in self.extensions.clone() {
                        ui.horizontal(|ui| {
                            let mut enabled = ext.enabled;
                            if ui.checkbox(&mut enabled, &ext.manifest.name).changed() {
                                if enabled && !ext.manifest.capabilities.is_empty() {
                                    self.pending_enable = Some(ext.clone());
                                } else if let Some(client) = &mut self.client {
                                    let _ = client.set_enabled(&ext.manifest.name, enabled);
                                }
                            }
                            ui.label(format!("{:?}", ext.last_status));
                        });
                    }
                }
                Tab::Browse => {
                    ui.heading("Browse");
                    ui.horizontal(|ui| {
                        ui.label("Repo index URL:");
                        egui::ComboBox::from_id_salt("repo_url_picker")
                            .selected_text(if self.repo_url.is_empty() { "(no repos configured)" } else { &self.repo_url })
                            .show_ui(ui, |ui| {
                                for repo in &self.gui_config.repos {
                                    ui.selectable_value(&mut self.repo_url, repo.clone(), repo);
                                }
                            });
                        if ui.add_enabled(!self.repo_url.is_empty(), egui::Button::new("Fetch")).clicked() {
                            self.store_index = Some(store::fetch_index(&self.repo_url));
                        }
                    });
                    match &self.store_index {
                        Some(Ok(index)) => {
                            for entry in index.extensions.clone() {
                                ui.horizontal(|ui| {
                                    ui.label(format!("{} v{}", entry.name, entry.version));
                                    if ui.button("Install").clicked() {
                                        let mut result = store::install(&entry, &extensions_dir());
                                        // The daemon only scans its extensions
                                        // directory at startup, so without this
                                        // the freshly installed extension would
                                        // not appear in the Installed tab (or be
                                        // enableable at all) until kenneld was
                                        // restarted by hand.
                                        if result.is_ok() {
                                            if let Some(client) = &mut self.client {
                                                if let Err(message) = client.rescan() {
                                                    result = Err(format!("installed, but the daemon could not be told to rescan: {message}"));
                                                }
                                            } else {
                                                result = Err("installed, but kenneld is not running -- it will pick this up when it next starts".to_string());
                                            }
                                        }
                                        self.install_status.insert(entry.name.clone(), result);
                                    }
                                    match self.install_status.get(&entry.name) {
                                        Some(Err(message)) => {
                                            ui.colored_label(egui::Color32::RED, message);
                                        }
                                        Some(Ok(())) => {
                                            ui.colored_label(egui::Color32::GREEN, "installed");
                                        }
                                        None => {}
                                    }
                                });
                            }
                        }
                        Some(Err(message)) => {
                            ui.colored_label(egui::Color32::RED, message);
                        }
                        None => {}
                    }
                }
                Tab::Settings => {
                    ui.heading("Settings");
                    ui.label("Extension repo URLs:");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.new_repo_url);
                        if ui.button("Add").clicked() && !self.new_repo_url.trim().is_empty() {
                            self.gui_config.repos.push(self.new_repo_url.trim().to_string());
                            let _ = self.gui_config.save(&gui_config_path());
                            self.new_repo_url.clear();
                        }
                    });
                    ui.separator();
                    let mut to_remove: Option<usize> = None;
                    for (i, repo) in self.gui_config.repos.clone().iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(repo);
                            if ui.button("Remove").clicked() {
                                to_remove = Some(i);
                            }
                        });
                    }
                    if let Some(i) = to_remove {
                        self.gui_config.repos.remove(i);
                        let _ = self.gui_config.save(&gui_config_path());
                        if !self.gui_config.repos.contains(&self.repo_url) {
                            self.repo_url = self.gui_config.repos.first().cloned().unwrap_or_default();
                        }
                    }
                }
            }
        });

        if let Some(ext) = self.pending_enable.clone() {
            egui::Window::new(format!("Allow {}?", ext.manifest.name)).collapsible(false).show(ctx, |ui| {
                ui.label(&ext.manifest.description);
                ui.separator();
                ui.label("This extension can:");
                for cap in &ext.manifest.capabilities {
                    ui.label(format!("• {:?}", cap));
                }
                if ext.manifest.capabilities.contains(&kennel_proto::Capability::PrivilegedSpawn) {
                    ui.separator();
                    ui.label("Root access requires this sudoers rule, installed by you (kennel will not write it):");
                    ui.code(format!(
                        "# /etc/sudoers.d/kennel-{}\n{}",
                        ext.manifest.name,
                        ext.manifest.privileged_commands.iter().map(|c| format!("{} ALL=(root) NOPASSWD: {}", std::env::var("USER").unwrap_or_default(), c)).collect::<Vec<_>>().join("\n")
                    ));
                }
                ui.horizontal(|ui| {
                    if ui.button("Allow").clicked() {
                        if let Some(client) = &mut self.client {
                            let _ = client.set_enabled(&ext.manifest.name, true);
                        }
                        self.pending_enable = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.pending_enable = None;
                    }
                });
            });
        }
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

fn main() -> eframe::Result<()> {
    eframe::run_native("kennel", eframe::NativeOptions::default(), Box::new(|_cc| Ok(Box::new(KennelApp::new()))))
}
