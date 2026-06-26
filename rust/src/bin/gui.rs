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

use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eframe::egui;
use egui_plot::{Bar, BarChart, Line, Plot, PlotPoints, Points};

use openspd_core::metrics::velocity_loss;
use openspd_core::profile::{self, default_v1rm, Lvp, Point};
use openspd_core::protocol::{
    start_command, ExerciseV1, Rep, DEFAULT_ROM_CM, ENCODER_HOST, ENCODER_PORT,
};
use openspd_io::RepEvent;

// Para dominada/fondo la carga es la TOTAL = peso corporal + lastre (ver profile::is_bodyweight).
const EXERCISES: [&str; 8] = [
    "sentadilla",
    "banca",
    "peso muerto",
    "press militar",
    "remo",
    "hip thrust",
    "dominada",
    "fondo",
];

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Acción de borrado elegida durante el render; se aplica tras cerrar los closures de egui
/// (no se puede mutar `self.log`/`self.points` mientras se itera para pintar).
enum DeleteAction {
    Series(u32),
    Rep(u32, usize),
}

struct GuiApp {
    exercise: String,
    v1rm: f64,
    load: f64,
    // ejercicios de peso corporal (dominada, fondo): la carga total = peso corporal + lastre
    bodyweight_kg: f64,
    added_load_kg: f64,
    rest_secs: f64,
    status: String,
    msg: String,
    set_idx: u32,
    current_set: Vec<Rep>,
    log: Vec<(u32, Rep, u64)>,
    points: Vec<Point>,
    // point_set[i] = set_idx que generó points[i]; None = punto precargado de un perfil (sin reps)
    point_set: Vec<Option<u32>>,
    lvp: Option<Lvp>,
    last_rep: Option<Instant>,
    // temporizador de preparación antes de iniciar la serie
    prep_secs: f64,
    countdown_until: Option<Instant>,
    csv_path: String,
    profile_path: String,
    // conexión (None = aún en pantalla de selección de encoder)
    rx: Option<Receiver<RepEvent>>,
    // escaneo BLE
    scanning: bool,
    scan_rx: Option<Receiver<Result<Vec<(String, String)>, String>>>,
    scan_results: Vec<(String, String)>,
    // fase del encoder v1: false = concéntrica (por defecto), true = excéntrica
    eccentric: bool,
    // audio: beeps de cuenta atrás y alarma al perder la velocidad objetivo
    _audio_stream: Option<rodio::OutputStream>, // mantener vivo o el sonido se corta
    audio: Option<rodio::OutputStreamHandle>,
    // pérdida de velocidad objetivo (%): al superarla suena la alarma
    vl_target: f64,
    last_count_shown: Option<i64>, // último número de la cuenta atrás que ya sonó
    vl_alarm_fired: bool,          // la alarma suena una sola vez por serie
    // multiusuario: usuario activo (slug), registro cacheado y buffer del campo "añadir"
    active_user: String,
    users: Vec<openspd_io::users::User>,
    new_user_buf: String,
    show_users: bool,
    // estado de sesión de cada usuario NO activo, para no perder series/reps al cambiar
    sessions: HashMap<String, UserSession>,
}

/// Estado de la sesión de un usuario que se conserva al cambiar de usuario (series, reps, perfil).
struct UserSession {
    exercise: String,
    v1rm: f64,
    set_idx: u32,
    current_set: Vec<Rep>,
    log: Vec<(u32, Rep, u64)>,
    points: Vec<Point>,
    point_set: Vec<Option<u32>>,
    lvp: Option<Lvp>,
    csv_path: String,
    profile_path: String,
}

impl GuiApp {
    fn refit(&mut self) {
        self.lvp = profile::fit(&self.exercise, self.points.clone(), self.v1rm);
    }

    /// Reproduce un tono senoidal generado por software (sin archivos de audio).
    /// Silencioso si no hay dispositivo de salida.
    fn play_tone(&self, freq: f32, ms: u64, amp: f32) {
        use rodio::Source;
        if let Some(h) = &self.audio {
            let src = rodio::source::SineWave::new(freq)
                .take_duration(Duration::from_millis(ms))
                .amplify(amp);
            let _ = h.play_raw(src.convert_samples());
        }
    }

    /// Carga total usada para el perfil. En ejercicios de peso corporal es peso corporal + lastre;
    /// en el resto, la carga única editable.
    fn current_load(&self) -> f64 {
        if profile::is_bodyweight(&self.exercise) {
            profile::total_bodyweight_load(self.bodyweight_kg, self.added_load_kg)
        } else {
            self.load
        }
    }

    fn finalize_set(&mut self) {
        if self.current_set.is_empty() {
            return;
        }
        let best = self.current_set.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
        let load = self.current_load();
        self.points.push(Point { load_kg: load, best_velocity: best });
        self.point_set.push(Some(self.set_idx));
        self.refit();
        self.msg = format!("Serie {} → punto ({:.1} kg, {:.2} m/s)", self.set_idx, load, best);
        self.set_idx += 1;
        self.current_set.clear();
        self.last_rep = None;
        self.vl_alarm_fired = false; // la próxima serie podrá volver a disparar la alarma
    }

    /// set_idx de las series ya cerradas (con reps en el log y set_idx < el de la serie en curso),
    /// en orden ascendente.
    fn finalized_set_ids(&self) -> Vec<u32> {
        let mut ids: Vec<u32> = Vec::new();
        for (s, _, _) in &self.log {
            if *s < self.set_idx && !ids.contains(s) {
                ids.push(*s);
            }
        }
        ids.sort_unstable();
        ids
    }

    /// Reps de una serie, en orden de registro.
    fn reps_of_set(&self, set_idx: u32) -> Vec<Rep> {
        self.log.iter().filter(|(s, _, _)| *s == set_idx).map(|(_, r, _)| *r).collect()
    }

    /// Carga registrada para una serie cerrada (la del punto del perfil), si existe.
    fn load_of_set(&self, set_idx: u32) -> Option<f64> {
        self.point_set.iter().position(|p| *p == Some(set_idx)).map(|i| self.points[i].load_kg)
    }

    /// Borra una serie cerrada entera y renumera correlativamente las posteriores.
    fn delete_series(&mut self, set_idx: u32) {
        self.log.retain(|(s, _, _)| *s != set_idx);
        for (s, _, _) in self.log.iter_mut() {
            if *s > set_idx {
                *s -= 1;
            }
        }
        if let Some(i) = self.point_set.iter().position(|p| *p == Some(set_idx)) {
            self.points.remove(i);
            self.point_set.remove(i);
        }
        for s in self.point_set.iter_mut().flatten() {
            if *s > set_idx {
                *s -= 1;
            }
        }
        if self.set_idx > 1 {
            self.set_idx -= 1;
        }
        self.refit();
        let _ = openspd_io::save_session_csv(&self.csv_path, &self.log);
        self.msg = format!("Serie {set_idx} eliminada (series renumeradas)");
    }

    /// Borra la rep `pos` (posición dentro de la serie) de una serie cerrada.
    fn delete_rep(&mut self, set_idx: u32, pos: usize) {
        let log_idx = self
            .log
            .iter()
            .enumerate()
            .filter(|(_, (s, _, _))| *s == set_idx)
            .nth(pos)
            .map(|(i, _)| i);
        let Some(log_idx) = log_idx else { return };
        self.log.remove(log_idx);
        // si es la serie en curso, mantener current_set en sincronía con el log
        if set_idx == self.set_idx && pos < self.current_set.len() {
            self.current_set.remove(pos);
        }

        let remaining = self.reps_of_set(set_idx);
        if remaining.is_empty() && set_idx != self.set_idx {
            self.delete_series(set_idx);
            return;
        }
        if let Some(i) = self.point_set.iter().position(|p| *p == Some(set_idx)) {
            let best = remaining.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
            self.points[i].best_velocity = best;
            self.refit();
        }
        let _ = openspd_io::save_session_csv(&self.csv_path, &self.log);
        self.msg = format!("Rep {} de la serie {set_idx} eliminada", pos + 1);
    }

    fn add_rep(&mut self, rep: Rep) {
        self.current_set.push(rep);
        self.log.push((self.set_idx, rep, now_unix()));
        self.last_rep = Some(Instant::now());
        let _ = openspd_io::save_session_csv(&self.csv_path, &self.log);
    }

    /// Arranca la cuenta atrás de preparación: limpia la serie en curso y, mientras
    /// está activa, las reps que llegan del encoder se descartan (no se cuentan).
    fn start_countdown(&mut self) {
        self.current_set.clear();
        self.last_rep = None;
        self.countdown_until = Some(Instant::now() + Duration::from_secs_f64(self.prep_secs));
        self.last_count_shown = None; // que el primer número de la cuenta atrás vuelva a sonar
        self.vl_alarm_fired = false;
        self.msg = format!("Prepárate… la serie empieza en {:.0} s", self.prep_secs);
    }

    /// Segundos restantes de la cuenta atrás, o `None` si no hay ninguna activa.
    fn countdown_remaining(&self) -> Option<f64> {
        self.countdown_until
            .map(|end| end.saturating_duration_since(Instant::now()).as_secs_f64())
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
            // el perfil es por (usuario, ejercicio): re-apuntar y recargar el del nuevo ejercicio
            self.load_user_profile();
        }
    }

    // ─────────────────────────── multiusuario ───────────────────────────

    /// Recarga el registro de usuarios desde disco.
    fn refresh_users(&mut self) {
        self.users = openspd_io::users::list_users().unwrap_or_default();
    }

    /// Nombre visible del usuario activo (cae al slug si no está en el registro).
    fn active_display(&self) -> String {
        self.users
            .iter()
            .find(|u| u.slug == self.active_user)
            .map(|u| u.display.clone())
            .unwrap_or_else(|| self.active_user.clone())
    }

    /// Añade el usuario tecleado en el campo, lo refresca y limpia el buffer.
    fn add_user_from_buf(&mut self) {
        let name = self.new_user_buf.trim().to_string();
        if name.is_empty() {
            return;
        }
        match openspd_io::users::add_user(&name) {
            Ok(u) => {
                self.new_user_buf.clear();
                self.refresh_users();
                self.msg = format!("Usuario añadido: {}", u.display);
            }
            Err(e) => self.msg = format!("No se pudo añadir el usuario: {e}"),
        }
    }

    /// Re-apunta `profile_path` al perfil del usuario+ejercicio actuales y lo carga (o limpia).
    fn load_user_profile(&mut self) {
        match openspd_io::users::profile_path_for(&self.active_user, &self.exercise) {
            Ok(p) => self.profile_path = p,
            Err(_) => self.profile_path = format!("{}.lvp", self.exercise),
        }
        match openspd_io::load_profile(&self.profile_path) {
            Ok(l) => {
                self.v1rm = l.v1rm;
                self.points = l.points.clone();
                self.point_set = vec![None; self.points.len()];
                self.lvp = Some(l);
            }
            Err(_) => {
                self.points.clear();
                self.point_set.clear();
                self.v1rm = default_v1rm(&self.exercise);
                self.refit();
            }
        }
    }

    /// Captura el estado de sesión del usuario activo (para conservarlo al cambiar).
    fn snapshot_session(&self) -> UserSession {
        UserSession {
            exercise: self.exercise.clone(),
            v1rm: self.v1rm,
            set_idx: self.set_idx,
            current_set: self.current_set.clone(),
            log: self.log.clone(),
            points: self.points.clone(),
            point_set: self.point_set.clone(),
            lvp: self.lvp.clone(),
            csv_path: self.csv_path.clone(),
            profile_path: self.profile_path.clone(),
        }
    }

    /// Restaura un estado de sesión previamente guardado (al volver a un usuario).
    fn restore_session(&mut self, s: UserSession) {
        self.exercise = s.exercise;
        self.v1rm = s.v1rm;
        self.set_idx = s.set_idx;
        self.current_set = s.current_set;
        self.log = s.log;
        self.points = s.points;
        self.point_set = s.point_set;
        self.lvp = s.lvp;
        self.csv_path = s.csv_path;
        self.profile_path = s.profile_path;
    }

    /// Inicia una sesión nueva para un usuario sin estado previo en esta ejecución: CSV nuevo y
    /// su perfil cargado de disco.
    fn init_fresh_session(&mut self, slug: &str) {
        self.set_idx = 1;
        self.current_set.clear();
        self.log.clear();
        self.csv_path = openspd_io::users::session_csv_path_for(slug, now_unix())
            .unwrap_or_else(|_| format!("sesion_{}.csv", now_unix()));
        self.load_user_profile();
    }

    /// Cambia de usuario activo conservando el estado de cada uno: guarda en disco y memoriza la
    /// sesión del usuario que se deja, y restaura (o inicia) la del usuario entrante. Nunca mezcla
    /// las series/reps de un usuario con las de otro.
    fn switch_user(&mut self, slug: &str) {
        if self.active_user == slug {
            self.show_users = false;
            return;
        }
        // volcar a disco y memorizar la sesión del usuario saliente
        self.save_all();
        let snap = self.snapshot_session();
        self.sessions.insert(self.active_user.clone(), snap);
        // timers/alarmas no se transfieren entre usuarios
        self.last_rep = None;
        self.countdown_until = None;
        self.last_count_shown = None;
        self.vl_alarm_fired = false;
        // entrar al nuevo usuario: restaurar su estado si ya entrenó en esta sesión, o iniciar uno
        self.active_user = slug.to_string();
        match self.sessions.remove(slug) {
            Some(s) => self.restore_session(s),
            None => self.init_fresh_session(slug),
        }
        self.show_users = false;
        self.msg = format!("Usuario activo: {}", self.active_display());
    }

    /// Ventana de gestión de usuarios: lista (con el activo marcado), añadir y quitar.
    fn users_window(&mut self, ctx: &egui::Context) {
        if !self.show_users {
            return;
        }
        let mut open = self.show_users;
        let mut switch_to: Option<String> = None;
        let mut remove: Option<String> = None;
        egui::Window::new("👤 Usuarios")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label("Cada usuario guarda sus series y su perfil por separado.");
                ui.separator();
                for u in self.users.clone() {
                    ui.horizontal(|ui| {
                        let is_active = u.slug == self.active_user;
                        let label = if is_active {
                            format!("● {}", u.display)
                        } else {
                            u.display.clone()
                        };
                        if ui.selectable_label(is_active, label).clicked() {
                            switch_to = Some(u.slug.clone());
                        }
                        // no permitir quitar el usuario activo
                        if !is_active && ui.small_button("🗑").on_hover_text("Quitar del registro").clicked() {
                            remove = Some(u.slug.clone());
                        }
                    });
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Nuevo:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.new_user_buf)
                            .hint_text("nombre del usuario"),
                    );
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Añadir").clicked() || enter {
                        self.add_user_from_buf();
                    }
                });
            });
        self.show_users = open;
        if let Some(slug) = switch_to {
            self.switch_user(&slug);
        } else if let Some(slug) = remove {
            let _ = openspd_io::users::remove_user(&slug, false);
            self.refresh_users();
            self.msg = "Usuario quitado del registro (sus ficheros se conservan)".into();
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

        // ¿hay una cuenta atrás de preparación activa? si acaba de expirar, la cerramos
        let counting_down = match self.countdown_until {
            Some(end) if Instant::now() < end => true,
            Some(_) => {
                self.countdown_until = None;
                self.last_count_shown = None;
                self.msg = "¡Ya! Empieza la serie.".into();
                self.play_tone(1320.0, 350, 0.25); // tono final, distinto del beep: ¡empieza!
                false
            }
            None => false,
        };

        // beep en cada número de la cuenta atrás (5·4·3·2·1), una sola vez por número
        if counting_down {
            if let Some(rem) = self.countdown_remaining() {
                let n = rem.ceil() as i64;
                if Some(n) != self.last_count_shown {
                    self.last_count_shown = Some(n);
                    self.play_tone(880.0, 120, 0.20);
                }
            }
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
                // durante la cuenta atrás descartamos las reps: te estás colocando en posición
                RepEvent::Rep(rep) => {
                    if !counting_down {
                        self.add_rep(rep);
                    }
                }
                RepEvent::Status(s) => self.status = s,
                RepEvent::Closed => self.status = "conexión cerrada por el encoder".into(),
            }
        }
        // alarma al superar la pérdida de velocidad objetivo de la serie (una vez por serie)
        if !counting_down && !self.current_set.is_empty() && !self.vl_alarm_fired {
            if velocity_loss(&self.current_set) >= self.vl_target {
                self.vl_alarm_fired = true;
                self.play_tone(440.0, 600, 0.30); // alarma grave y larga, distinta del beep
            }
        }

        // cierre de serie por descanso (no mientras nos preparamos)
        if !counting_down {
            if let Some(t) = self.last_rep {
                if !self.current_set.is_empty() && t.elapsed().as_secs_f64() >= self.rest_secs {
                    self.finalize_set();
                }
            }
        }
        // repintar aunque no haya eventos (para refrescar reps que llegan del hilo)
        ctx.request_repaint_after(Duration::from_millis(100));

        self.top_bar(ctx);
        self.controls(ctx);
        self.profile_panel(ctx);
        self.current_set_panel(ctx);
        self.users_window(ctx);
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
                let mode = ExerciseV1::from_name(&self.exercise).unwrap_or(ExerciseV1::Bench);
                ui.horizontal(|ui| {
                    ui.label("Captura:");
                    ui.label(egui::RichText::new(mode.label()).color(egui::Color32::LIGHT_BLUE));
                    ui.separator();
                    ui.label("Fase:");
                    ui.selectable_value(&mut self.eccentric, false, "concéntrica");
                    ui.selectable_value(&mut self.eccentric, true, "excéntrica");
                });
                if ui.button(format!("▶ Conectar (TCP {ENCODER_HOST}:{ENCODER_PORT})")).clicked() {
                    self.status = "conectando (v1)…".into();
                    // Modo del encoder = ejercicio elegido (fallback banca); fase = self.eccentric.
                    // Modo no-tiempo-real (const 7) → concéntrica limpia vía sondeo de ?8.
                    let command = start_command(mode, DEFAULT_ROM_CM, self.eccentric);
                    self.rx = Some(openspd_io::spawn_tcp_reader(
                        ENCODER_HOST.to_string(),
                        ENCODER_PORT,
                        command,
                    ));
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
                if ui.button("👤 Usuarios").clicked() {
                    self.refresh_users();
                    self.show_users = !self.show_users;
                }
                ui.label(format!("Usuario: {}", self.active_display()));
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
        self.countdown_until = None;
        self.last_count_shown = None;
        self.vl_alarm_fired = false;
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
                if profile::is_bodyweight(&self.exercise) {
                    // peso corporal + lastre (el lastre puede ser negativo si hay asistencia)
                    ui.label("Peso corporal (kg):");
                    ui.add(egui::DragValue::new(&mut self.bodyweight_kg).speed(0.5).range(0.0..=300.0).suffix(" kg"));
                    ui.label("Lastre (kg):");
                    if ui.button("−2.5").clicked() { self.added_load_kg -= 2.5; }
                    ui.add(egui::DragValue::new(&mut self.added_load_kg).speed(0.5).range(-100.0..=300.0).suffix(" kg"));
                    if ui.button("+2.5").clicked() { self.added_load_kg += 2.5; }
                    ui.label(egui::RichText::new(format!("= {:.1} kg total", self.current_load())).strong());
                } else {
                    ui.label("Carga (kg):");
                    if ui.button("−10").clicked() { self.load = (self.load - 10.0).max(0.0); }
                    if ui.button("−2.5").clicked() { self.load = (self.load - 2.5).max(0.0); }
                    ui.add(egui::DragValue::new(&mut self.load).speed(0.5).range(0.0..=600.0).suffix(" kg"));
                    if ui.button("+2.5").clicked() { self.load += 2.5; }
                    if ui.button("+10").clicked() { self.load += 10.0; }
                }
                ui.separator();
                if ui.button("▶ Iniciar serie (cuenta atrás)").clicked() { self.start_countdown(); }
                if ui.button("Cerrar serie").clicked() { self.finalize_set(); }
                if ui.button("Deshacer punto").clicked() {
                    if self.points.pop().is_some() {
                        self.point_set.pop();
                        self.refit();
                        self.msg = "Último punto eliminado".into();
                    }
                }
                if ui.button("💾 Guardar").clicked() { self.save_all(); }
            });
            ui.horizontal(|ui| {
                ui.label("Preparación (cuenta atrás):");
                ui.add(egui::DragValue::new(&mut self.prep_secs).speed(1.0).range(0.0..=30.0).suffix(" s"));
                ui.separator();
                ui.label("Descanso para cerrar serie:");
                ui.add(egui::DragValue::new(&mut self.rest_secs).speed(1.0).range(3.0..=180.0).suffix(" s"));
                ui.separator();
                ui.label("Pérdida objetivo (alarma):");
                ui.add(egui::DragValue::new(&mut self.vl_target).speed(1.0).range(5.0..=50.0).suffix(" %"));
                ui.separator();
                ui.label(egui::RichText::new(&self.msg).weak());
            });
        });
    }

    fn current_set_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // si hay cuenta atrás de preparación activa, la mostramos bien visible
            if let Some(rem) = self.countdown_remaining() {
                ui.add_space(20.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("PREPÁRATE")
                            .size(28.0)
                            .strong()
                            .color(egui::Color32::from_rgb(220, 60, 60)),
                    );
                    ui.label(
                        egui::RichText::new(format!("{:.0}", rem.ceil()))
                            .size(96.0)
                            .strong()
                            .color(egui::Color32::from_rgb(220, 60, 60)),
                    );
                    ui.label(egui::RichText::new("las repeticiones aún no cuentan").weak());
                });
                return;
            }

            ui.heading(format!("Serie actual · {} reps · {:.1} kg", self.current_set.len(), self.current_load()));

            // velocity loss (el umbral rojo coincide con la pérdida objetivo / alarma)
            let vl = velocity_loss(&self.current_set);
            let color = if vl >= self.vl_target {
                egui::Color32::from_rgb(220, 60, 60)
            } else if vl >= self.vl_target * 0.6 {
                egui::Color32::from_rgb(220, 180, 60)
            } else {
                egui::Color32::from_rgb(60, 200, 100)
            };
            ui.horizontal(|ui| {
                ui.label("Velocity loss:");
                ui.add(egui::ProgressBar::new((vl / (self.vl_target * 1.5).max(1.0)).clamp(0.0, 1.0) as f32)
                    .text(format!("{vl:.1} % / {:.0} %", self.vl_target))
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

            // acción de borrado elegida en este frame (se aplica al final)
            let mut pending: Option<DeleteAction> = None;

            // tabla de reps de la serie actual (con ✕ para borrar la rep)
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("reps").striped(true).num_columns(6).show(ui, |ui| {
                    for h in ["rep", "media (m/s)", "ROM (cm)", "pico (m/s)", "VL %", ""] {
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
                        if ui.small_button("🗑").on_hover_text("Eliminar esta rep").clicked() {
                            pending = Some(DeleteAction::Rep(self.set_idx, i));
                        }
                        ui.end_row();
                    }
                });

                // revisión en línea de las series anteriores (cerradas)
                let ids = self.finalized_set_ids();
                if !ids.is_empty() {
                    ui.separator();
                    ui.heading("Series anteriores");
                    for set_idx in ids {
                        let reps = self.reps_of_set(set_idx);
                        let best = reps.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
                        let load = self.load_of_set(set_idx);
                        let title = format!(
                            "Serie {set_idx} · {} · {} reps · mejor {best:.2} m/s",
                            load.map(|l| format!("{l:.1} kg")).unwrap_or_else(|| "— kg".into()),
                            reps.len(),
                        );
                        egui::CollapsingHeader::new(title).id_salt(set_idx).show(ui, |ui| {
                            if ui.button("🗑 Eliminar serie").clicked() {
                                pending = Some(DeleteAction::Series(set_idx));
                            }
                            egui::Grid::new(("hist", set_idx)).striped(true).num_columns(5).show(ui, |ui| {
                                for h in ["rep", "media (m/s)", "ROM (cm)", "pico (m/s)", ""] {
                                    ui.strong(h);
                                }
                                ui.end_row();
                                for (i, r) in reps.iter().enumerate() {
                                    ui.label(format!("{}", i + 1));
                                    ui.label(format!("{:.2}", r.mean_velocity));
                                    ui.label(format!("{:.1}", r.rom));
                                    ui.label(format!("{:.2}", r.peak_velocity));
                                    if ui.small_button("🗑").on_hover_text("Eliminar esta rep").clicked() {
                                        pending = Some(DeleteAction::Rep(set_idx, i));
                                    }
                                    ui.end_row();
                                }
                            });
                        });
                    }
                }
            });

            // aplicar el borrado fuera de los closures de pintado
            match pending {
                Some(DeleteAction::Series(s)) => self.delete_series(s),
                Some(DeleteAction::Rep(s, p)) => self.delete_rep(s, p),
                None => {}
            }
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
    bodyweight: f64,
    added_load: f64,
    prep: f64,
    csv: Option<String>,
    profile_path: Option<String>,
    loaded: Option<Lvp>,
    user: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args {
        exercise: "sentadilla".into(),
        load: 20.0,
        bodyweight: 75.0,
        added_load: 0.0,
        prep: 5.0,
        csv: None,
        profile_path: None,
        loaded: None,
        user: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--exercise" => a.exercise = it.next().unwrap_or(a.exercise),
            "--load" => a.load = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.load),
            "--bodyweight" => a.bodyweight = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.bodyweight),
            "--added-load" | "--lastre" => a.added_load = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.added_load),
            "--prep" => a.prep = it.next().and_then(|v| v.parse().ok()).unwrap_or(a.prep),
            "--user" => a.user = it.next(),
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
    // usuario activo: --user o "default" (creado de forma idempotente)
    let user = openspd_io::users::add_user(args.user.as_deref().unwrap_or("default"))
        .unwrap_or(openspd_io::users::User { slug: "default".into(), display: "default".into() });

    // perfil: un --profile explícito gana; si no, el del (usuario, ejercicio) actual
    let (profile_path, loaded) = match (args.profile_path, args.loaded) {
        (Some(p), l) => (p, l),
        (None, _) => {
            let p = openspd_io::users::profile_path_for(&user.slug, &args.exercise)
                .unwrap_or_else(|_| format!("{}.lvp", args.exercise));
            let l = openspd_io::load_profile(&p).ok();
            (p, l)
        }
    };
    let csv_path = args.csv.unwrap_or_else(|| {
        openspd_io::users::session_csv_path_for(&user.slug, now_unix())
            .unwrap_or_else(|_| format!("sesion_{}.csv", now_unix()))
    });
    let exercise = loaded.as_ref().map(|l| l.exercise.clone()).unwrap_or(args.exercise);
    let v1rm = loaded.as_ref().map(|l| l.v1rm).unwrap_or_else(|| default_v1rm(&exercise));
    let points = loaded.as_ref().map(|l| l.points.clone()).unwrap_or_default();
    let point_set = vec![None; points.len()]; // los puntos precargados no tienen reps en el log
    let lvp = loaded;
    let users = openspd_io::users::list_users().unwrap_or_default();

    // salida de audio para los beeps/alarma; si no hay dispositivo, el GUI va en silencio
    let (audio_stream, audio_handle) = match rodio::OutputStream::try_default() {
        Ok((s, h)) => (Some(s), Some(h)),
        Err(_) => (None, None),
    };

    let app = GuiApp {
        exercise,
        v1rm,
        load: args.load,
        bodyweight_kg: args.bodyweight,
        added_load_kg: args.added_load,
        rest_secs: 20.0,
        prep_secs: args.prep,
        countdown_until: None,
        status: "elige un encoder para empezar".into(),
        msg: "Seleccionar un encoder. Tras conectar: fijar carga, hacer la serie y descansar (o 'Cerrar serie').".into(),
        set_idx: 1,
        current_set: Vec::new(),
        log: Vec::new(),
        points,
        point_set,
        lvp,
        last_rep: None,
        csv_path,
        profile_path,
        rx: None,
        scanning: false,
        scan_rx: None,
        scan_results: Vec::new(),
        eccentric: false,
        _audio_stream: audio_stream,
        audio: audio_handle,
        vl_target: 20.0,
        last_count_shown: None,
        vl_alarm_fired: false,
        active_user: user.slug,
        users,
        new_user_buf: String::new(),
        show_users: false,
        sessions: HashMap::new(),
    };

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };
    eframe::run_native("OpenSPD", opts, Box::new(|_cc| Ok(Box::new(app))))
}
