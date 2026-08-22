mod client;

use client::Client;
use eframe::egui;
use kennel_proto::ExtensionInfo;
use std::path::PathBuf;

fn socket_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support/kennel/control.sock")
}

struct KennelApp {
    client: Option<Client>,
    extensions: Vec<ExtensionInfo>,
}

impl KennelApp {
    fn new() -> Self {
        let client = Client::connect(&socket_path()).ok();
        KennelApp { client, extensions: vec![] }
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
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Installed");
            if self.client.is_none() {
                ui.label("kenneld is not running");
                return;
            }
            for ext in self.extensions.clone() {
                ui.horizontal(|ui| {
                    let mut enabled = ext.enabled;
                    if ui.checkbox(&mut enabled, &ext.manifest.name).changed() {
                        if let Some(client) = &mut self.client {
                            let _ = client.set_enabled(&ext.manifest.name, enabled);
                        }
                    }
                    ui.label(format!("{:?}", ext.last_status));
                });
            }
        });
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

fn main() -> eframe::Result<()> {
    eframe::run_native("kennel", eframe::NativeOptions::default(), Box::new(|_cc| Ok(Box::new(KennelApp::new()))))
}
