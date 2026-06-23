// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! GUI nativa (egui/eframe) para el encoder VBT.
//!
//! Reutiliza la misma logica que el CLI/TUI (crate `openspd`): un hilo abre el socket TCP y
//! manda cada repeticion por un canal; la UI lo pinta en vivo. Incluye:
//!   - Serie actual: tabla de reps, velocity loss y grafica de barras de velocidad.
//!   - Perfil carga-velocidad: scatter de puntos + recta de regresion + 1RM/R² en vivo.
//!
//! Uso:  cargo run --release --bin openspd-gui
//!       (opcional)  --exercise sentadilla  --load 40  --profile sentadilla.lvp

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui_plot::{Bar, BarChart, Line, Plot, PlotPoints, Points};

use openspd_core::metrics::velocity_loss;
use openspd_core::profile::{self, default_v1rm, Lvp, Point};
use openspd_core::protocol::{Rep, ENCODER_HOST, ENCODER_PORT};
use openspd_io::RepEvent;

const EXERCISES: [&str; 3] = ["sentadilla", "banca", "peso muerto"];

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct GuiApp {
    exercise: String,
    v1rm: f64,
    load: f64,
    rest_secs: f64,
    status: String,
    msg: String,
    set_idx: u32,
    current_set: Vec<Rep>,
    log: Vec<(u32, Rep, u64)>,
    points: Vec<Point>,
    lvp: Option<Lvp>,
    last_rep: Option<Instant>,
    csv_path: String,
    profile_path: String,
    // conexión (None = aún en pantalla de selección de encoder)
    rx: Option<Receiver<RepEvent>>,
    // escaneo BLE
    scanning: bool,
    scan_rx: Option<Receiver<Result<Vec<(String, String)>, String>>>,
    scan_results: Vec<(String, String)>,
}

impl GuiApp {
    fn refit(&mut self) {
        self.lvp = profile::fit(&self.exercise, self.points.clone(), self.v1rm);
    }

    fn finalize_set(&mut self) {
        if self.current_set.is_empty() {
            return;
        }
        let best = self.current_set.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
        self.points.push(Point { load_kg: self.load, best_velocity: best });
        self.refit();
        self.msg = format!("Serie {} → punto ({:.1} kg, {:.2} m/s)", self.set_idx, self.load, best);
        self.set_idx += 1;
        self.current_set.clear();
        self.last_rep = None;
    }

    fn add_rep(&mut self, rep: Rep) {
        self.current_set.push(rep);
        self.log.push((self.set_idx, rep, now_unix()));
        self.last_rep = Some(Instant::now());
        let _ = openspd_io::save_session_csv(&self.csv_path, &self.log);
    }

    fn save_all(&mut self) {
        let _ = openspd_io::save_session_csv(&self.csv_path, &self.log);
        match &self.lvp {
            Some(lvp) => match openspd_io::save_profile(&self.profile_path, lvp) {
                Ok(_) => self.msg = format!("Guardado: {} y {}", self.csv_path, self.profile_path),
                Err(e) => self.msg = format!("Error guardando perfil: {e}"),
            },
            None => self.msg = format!("CSV guardado en {} (perfil necesita ≥2 cargas)", self.csv_path),
        }
    }

    fn set_exercise(&mut self, ex: &str) {
        if self.exercise != ex {
            self.exercise = ex.to_string();
            self.v1rm = default_v1rm(ex);
            self.refit();
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // recoger resultados del escaneo BLE
        if let Some(srx) = &self.scan_rx {
            if let Ok(res) = srx.try_recv() {
                self.scanning = false;
                match res {
                    Ok(list) => {
                        self.scan_results = list;
                        if self.scan_results.is_empty() {
                            self.msg = "No se encontraron encoders v2 (¿encendido? ¿BT del móvil apagado?)".into();
                        }
                    }
                    Err(e) => self.msg = format!("escaneo: {e}"),
                }
                self.scan_rx = None;
            }
        }

        // pantalla de selección si aún no hay conexión
        if self.rx.is_none() {
            self.select_screen(ctx);
            ctx.request_repaint_after(Duration::from_millis(150));
            return;
        }

        // drenar canal de la conexión activa (recoger primero para evitar conflicto de borrow)
        let mut incoming = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(m) = rx.try_recv() {
                incoming.push(m);
            }
        }
        for m in incoming {
            match m {
                RepEvent::Rep(rep) => self.add_rep(rep),
                RepEvent::Status(s) => self.status = s,
                RepEvent::Closed => self.status = "conexión cerrada por el encoder".into(),
            }
        }
        // cierre de serie por descanso
        if let Some(t) = self.last_rep {
            if !self.current_set.is_empty() && t.elapsed().as_secs_f64() >= self.rest_secs {
                self.finalize_set();
            }
        }
        // repintar aunque no haya eventos (para refrescar reps que llegan del hilo)
        ctx.request_repaint_after(Duration::from_millis(100));

        self.top_bar(ctx);
        self.controls(ctx);
        self.profile_panel(ctx);
        self.current_set_panel(ctx);
    }
}

impl GuiApp {
    fn select_screen(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("OpenSPD — elige el encoder");
            ui.add_space(12.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("Encoder v1 — WiFi").strong());
                ui.label("Requiere estar conectado al AP del encoder (ver README).");
                if ui.button(format!("▶ Conectar (TCP {ENCODER_HOST}:{ENCODER_PORT})")).clicked() {
                    self.status = "conectando (v1)…".into();
                    self.rx = Some(openspd_io::spawn_tcp_reader(ENCODER_HOST.to_string(), ENCODER_PORT));
                }
            });

            ui.add_space(10.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("Encoder v2 — BLE").strong());
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.scanning, egui::Button::new("🔍 Buscar encoders")).clicked() {
                        self.scanning = true;
                        self.scan_results.clear();
                        self.msg = "buscando encoders v2…".into();
                        self.scan_rx = Some(openspd_io::spawn_ble_scan(openspd_io::DEFAULT_SCAN_SECS));
                    }
                    if self.scanning {
                        ui.spinner();
                        ui.label("buscando (≈6 s)…");
                    }
                });
                for (addr, label) in self.scan_results.clone() {
                    if ui.button(format!("▶ Conectar a {label}")).clicked() {
                        self.status = format!("conectando (v2) a {addr}…");
                        self.rx = Some(openspd_io::spawn_ble_reader(addr));
                    }
                }
            });

            ui.add_space(12.0);
            ui.weak(&self.msg);
        });
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("OpenSPD");
                if ui.button("⟵ Cambiar encoder").clicked() {
                    self.disconnect_to_select();
                }
                ui.separator();
                ui.label(format!("Serie {}", self.set_idx));
                ui.separator();
                ui.label(egui::RichText::new(self.status.clone()).color(egui::Color32::LIGHT_BLUE));
            });
        });
    }

    /// Cierra la conexión actual y vuelve a la pantalla de selección de encoder.
    fn disconnect_to_select(&mut self) {
        self.rx = None; // al soltar el Receiver, el hilo lector termina en su próximo envío
        self.current_set.clear();
        self.last_rep = None;
        self.scanning = false;
        self.scan_rx = None;
        self.scan_results.clear();
        self.status = "elige un encoder".into();
        self.msg = "Desconectado. Elige un encoder. (El perfil y las series guardadas se conservan.)".into();
    }

    fn controls(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("controls").show(ctx, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label("Ejercicio:");
                let cur = self.exercise.clone();
                egui::ComboBox::from_id_salt("ex")
                    .selected_text(&cur)
                    .show_ui(ui, |ui| {
                        for ex in EXERCISES {
                            if ui.selectable_label(cur == ex, ex).clicked() {
                                self.set_exercise(ex);
                            }
                        }
                    });
                ui.separator();
                ui.label("Carga (kg):");
                if ui.button("−10").clicked() { self.load = (self.load - 10.0).max(0.0); }
                if ui.button("−2.5").clicked() { self.load = (self.load - 2.5).max(0.0); }
                ui.add(egui::DragValue::new(&mut self.load).speed(0.5).range(0.0..=600.0).suffix(" kg"));
                if ui.button("+2.5").clicked() { self.load += 2.5; }
                if ui.button("+10").clicked() { self.load += 10.0; }
                ui.separator();
                if ui.button("Cerrar serie").clicked() { self.finalize_set(); }
                if ui.button("Deshacer punto").clicked() {
                    if self.points.pop().is_some() {
                        self.refit();
                        self.msg = "Último punto eliminado".into();
                    }
                }
                if ui.button("💾 Guardar").clicked() { self.save_all(); }
            });
            ui.horizontal(|ui| {
                ui.label("Descanso para cerrar serie:");
                ui.add(egui::DragValue::new(&mut self.rest_secs).speed(1.0).range(3.0..=180.0).suffix(" s"));
                ui.separator();
                ui.label(egui::RichText::new(&self.msg).weak());
            });
        });
    }

    fn current_set_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(format!("Serie actual · {} reps · {:.1} kg", self.current_set.len(), self.load));

            // velocity loss
            let vl = velocity_loss(&self.current_set);
            let color = if vl >= 30.0 {
                egui::Color32::from_rgb(220, 60, 60)
            } else if vl >= 20.0 {
                egui::Color32::from_rgb(220, 180, 60)
            } else {
                egui::Color32::from_rgb(60, 200, 100)
            };
            ui.horizontal(|ui| {
                ui.label("Velocity loss:");
                ui.add(egui::ProgressBar::new((vl / 40.0).clamp(0.0, 1.0) as f32)
                    .text(format!("{vl:.1} %"))
                    .fill(color));
            });

            ui.separator();

            // grafica de barras: velocidad media por rep
            let bars: Vec<Bar> = self.current_set.iter().enumerate()
                .map(|(i, r)| Bar::new((i + 1) as f64, r.mean_velocity))
                .collect();
            Plot::new("vel_bars")
                .height(160.0)
                .allow_drag(false).allow_zoom(false).allow_scroll(false)
                .show(ui, |pui| {
                    pui.bar_chart(BarChart::new(bars).color(egui::Color32::from_rgb(80, 160, 230)));
                });

            ui.separator();

            // tabla de reps
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("reps").striped(true).num_columns(5).show(ui, |ui| {
                    for h in ["rep", "media (m/s)", "ROM (cm)", "pico (m/s)", "VL %"] {
                        ui.strong(h);
                    }
                    ui.end_row();
                    for (i, r) in self.current_set.iter().enumerate() {
                        let vl = velocity_loss(&self.current_set[..=i]);
                        ui.label(format!("{}", r.rep));
                        ui.label(format!("{:.2}", r.mean_velocity));
                        ui.label(format!("{:.1}", r.rom));
                        ui.label(format!("{:.2}", r.peak_velocity));
                        ui.label(format!("{:.1}", vl));
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn profile_panel(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("profile").default_width(380.0).show(ctx, |ui| {
            ui.heading("Perfil carga-velocidad");

            match &self.lvp {
                Some(l) if l.is_valid() => {
                    ui.label(egui::RichText::new(format!("1RM ≈ {:.0} kg", l.one_rm_kg))
                        .size(22.0).strong().color(egui::Color32::from_rgb(60, 200, 100)));
                    ui.label(format!("v = {:.3} {:+.4}·carga", l.intercept, l.slope));
                    ui.label(format!("R² = {:.3}   ·   V1RM = {:.2} m/s", l.r2, l.v1rm));
                    ui.label("Velocidad objetivo por %1RM:");
                    let t: Vec<String> = [100.0, 90.0, 80.0, 70.0, 60.0]
                        .iter().map(|p| format!("{:.0}%→{:.2}", p, l.velocity_for_pct(*p))).collect();
                    ui.label(t.join("   "));
                }
                Some(_) => { ui.colored_label(egui::Color32::RED, "perfil incoherente (¿velocidad no baja con la carga?)"); }
                None => { ui.weak("Añade ≥2 cargas distintas (haz una serie, cambia la carga, otra serie…)"); }
            }

            ui.separator();

            // scatter de puntos + recta
            let pts: Vec<[f64; 2]> = self.points.iter().map(|p| [p.load_kg, p.best_velocity]).collect();
            let line_pts: Option<Vec<[f64; 2]>> = self.lvp.as_ref().filter(|l| l.is_valid()).map(|l| {
                let x0 = 0.0_f64;
                let x1 = l.one_rm_kg.max(self.points.iter().map(|p| p.load_kg).fold(0.0, f64::max));
                vec![[x0, l.intercept + l.slope * x0], [x1, l.intercept + l.slope * x1]]
            });

            Plot::new("lvp_plot")
                .height(260.0)
                .x_axis_label("carga (kg)")
                .y_axis_label("velocidad media (m/s)")
                .show(ui, |pui| {
                    if let Some(lp) = line_pts {
                        pui.line(Line::new(PlotPoints::from(lp))
                            .color(egui::Color32::from_rgb(220, 180, 60)).name("ajuste"));
                    }
                    pui.points(Points::new(PlotPoints::from(pts))
                        .radius(5.0).color(egui::Color32::from_rgb(80, 160, 230)).name("series"));
                });

            ui.separator();
            ui.weak(format!("CSV: {}", self.csv_path));
            ui.weak(format!("Perfil: {}", self.profile_path));
            ui.weak("OpenSPD · GPLv3 · software no oficial, sin relación con Speed4lifts/Vitruve");
        });
    }
}

struct Args {
    exercise: String,
    load: f64,
    csv: Option<String>,
    profile_path: Option<String>,
    loaded: Option<Lvp>,
}

fn parse_args() -> Args {
    let mut a = Args {
        exercise: "sentadilla".into(),
        load: 20.0,
        csv: None,
        profile_path: None,
        loaded: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--exercise" => a.exercise = it.next().unwrap_or(a.exercise),
            "--load" => a.load = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.load),
            "--csv" => a.csv = it.next(),
            "--profile" => {
                if let Some(p) = it.next() {
                    if let Ok(l) = openspd_io::load_profile(&p) {
                        a.exercise = l.exercise.clone();
                        a.loaded = Some(l);
                    }
                    a.profile_path = Some(p);
                }
            }
            _ => {}
        }
    }
    a
}

fn main() -> eframe::Result<()> {
    let args = parse_args();
    let csv_path = args.csv.unwrap_or_else(|| format!("sesion_{}.csv", now_unix()));
    let profile_path = args.profile_path.unwrap_or_else(|| format!("{}.lvp", args.exercise));
    let v1rm = args.loaded.as_ref().map(|l| l.v1rm).unwrap_or_else(|| default_v1rm(&args.exercise));
    let points = args.loaded.as_ref().map(|l| l.points.clone()).unwrap_or_default();
    let lvp = args.loaded;

    let app = GuiApp {
        exercise: args.exercise,
        v1rm,
        load: args.load,
        rest_secs: 20.0,
        status: "elige un encoder para empezar".into(),
        msg: "Elige el encoder. Tras conectar: pon carga, haz la serie y descansa (o 'Cerrar serie').".into(),
        set_idx: 1,
        current_set: Vec::new(),
        log: Vec::new(),
        points,
        lvp,
        last_rep: None,
        csv_path,
        profile_path,
        rx: None,
        scanning: false,
        scan_rx: None,
        scan_results: Vec::new(),
    };

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native("OpenSPD", opts, Box::new(|_cc| Ok(Box::new(app))))
}
