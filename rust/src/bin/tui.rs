// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! TUI para el encoder VBT: dashboard en vivo + constructor de perfil carga-velocidad.
//!
//! Flujo para construir tu perfil:
//!   1) Ajusta la carga (kg) con +/-  (paso 2.5) o  [ ]  (paso 10). En dominada/fondo (peso
//!      corporal) esas teclas editan el LASTRE; el peso corporal se fija con --bodyweight y la
//!      carga total = peso corporal + lastre.
//!   2) Haz la serie a esa carga. Cada rep aparece en vivo.
//!   3) Descansa (o pulsa 'c'): la serie se cierra y se añade un punto (carga, mejor velocidad)
//!      al perfil. Repite con 2-3 cargas distintas y el perfil se ajusta solo (1RM, R²).
//!   4) 's' guarda perfil + CSV. 'u' deshace el último punto. 'q' sale (guarda).
//!
//! Uso:
//!   openspd-tui --exercise sentadilla --load 40
//!   openspd-tui --exercise dominada --bodyweight 78 --lastre 20   (carga total = 98 kg)
//!   openspd-tui --profile sentadilla.lvp        (carga un perfil existente para seguir)

use std::io;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};

use openspd_core::metrics::velocity_loss;
use openspd_core::profile::{self, default_v1rm, Lvp, Point};
use openspd_core::protocol::{Rep, ENCODER_HOST, ENCODER_PORT};
use openspd_io::RepEvent;

struct App {
    exercise: String,
    v1rm: f64,
    load: f64,
    // ejercicios de peso corporal (dominada, fondo): la carga total = peso corporal + lastre
    bodyweight_kg: f64,
    added_load_kg: f64,
    status: String,
    set_idx: u32,
    current_set: Vec<Rep>,
    log: Vec<(u32, Rep, u64)>,
    points: Vec<Point>,
    // point_set[i] = set_idx que generó points[i]; None = punto precargado de un perfil (sin reps)
    point_set: Vec<Option<u32>>,
    lvp: Option<Lvp>,
    // revisión de series anteriores: serie seleccionada y, opcionalmente, rep dentro de ella
    hist_sel_set: usize,
    hist_sel_rep: Option<usize>,
    last_rep: Option<Instant>,
    rest: Duration,
    // temporizador de preparación antes de iniciar la serie
    prep_secs: f64,
    countdown_until: Option<Instant>,
    csv_path: String,
    profile_path: String,
    msg: String,
    quit: bool,
    // conexión (None = pantalla de selección de encoder)
    rx: Option<Receiver<RepEvent>>,
    scanning: bool,
    scan_rx: Option<Receiver<Result<Vec<(String, String)>, String>>>,
    scan_results: Vec<(String, String)>,
}

impl App {
    fn refit(&mut self) {
        self.lvp = profile::fit(&self.exercise, self.points.clone(), self.v1rm);
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

    /// Ajusta la carga editable con las teclas: el lastre en peso corporal (admite negativo por
    /// asistencia), o la carga total en el resto (acotada a >=0).
    fn adjust_load(&mut self, delta: f64) {
        if profile::is_bodyweight(&self.exercise) {
            self.added_load_kg += delta;
        } else {
            self.load = (self.load + delta).max(0.0);
        }
    }

    fn finalize_set(&mut self) {
        if self.current_set.is_empty() {
            return;
        }
        let best = self
            .current_set
            .iter()
            .map(|r| r.mean_velocity)
            .fold(f64::MIN, f64::max);
        let load = self.current_load();
        self.points.push(Point { load_kg: load, best_velocity: best });
        self.point_set.push(Some(self.set_idx));
        self.refit();
        self.msg = format!(
            "Serie {} cerrada → punto ({:.1} kg, {:.2} m/s)",
            self.set_idx, load, best
        );
        self.set_idx += 1;
        self.current_set.clear();
        self.last_rep = None;
    }

    /// set_idx de las series ya cerradas (con reps en el log y set_idx < el de la serie en curso),
    /// en orden ascendente. La serie en curso no aparece: se revisa/edita en la vista en vivo.
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

    /// Reps de una serie (posición dentro de la serie + la rep), en orden de registro.
    fn reps_of_set(&self, set_idx: u32) -> Vec<Rep> {
        self.log.iter().filter(|(s, _, _)| *s == set_idx).map(|(_, r, _)| *r).collect()
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

        let remaining = self.reps_of_set(set_idx);
        if remaining.is_empty() {
            // sin reps: la serie desaparece (y se renumera)
            self.delete_series(set_idx);
            return;
        }
        // recomputar la mejor velocidad del punto de esa serie
        if let Some(i) = self.point_set.iter().position(|p| *p == Some(set_idx)) {
            let best = remaining.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
            self.points[i].best_velocity = best;
            self.refit();
        }
        let _ = openspd_io::save_session_csv(&self.csv_path, &self.log);
        self.msg = format!("Rep {} de la serie {set_idx} eliminada", pos + 1);
    }

    /// Acota la selección del historial tras un borrado o cuando cambian las series.
    fn clamp_history_selection(&mut self) {
        let ids = self.finalized_set_ids();
        if ids.is_empty() {
            self.hist_sel_set = 0;
            self.hist_sel_rep = None;
            return;
        }
        if self.hist_sel_set >= ids.len() {
            self.hist_sel_set = ids.len() - 1;
        }
        if let Some(rp) = self.hist_sel_rep {
            let n = self.reps_of_set(ids[self.hist_sel_set]).len();
            if n == 0 {
                self.hist_sel_rep = None;
            } else if rp >= n {
                self.hist_sel_rep = Some(n - 1);
            }
        }
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
        self.msg = format!("Prepárate… la serie empieza en {:.0} s", self.prep_secs);
    }

    /// Segundos restantes de la cuenta atrás, o `None` si no hay ninguna activa.
    fn countdown_remaining(&self) -> Option<f64> {
        self.countdown_until
            .map(|end| end.saturating_duration_since(Instant::now()).as_secs_f64())
    }

    fn save_all(&mut self) {
        let _ = openspd_io::save_session_csv(&self.csv_path, &self.log);
        if let Some(lvp) = &self.lvp {
            match openspd_io::save_profile(&self.profile_path, lvp) {
                Ok(_) => self.msg = format!("Guardado: {} y {}", self.csv_path, self.profile_path),
                Err(e) => self.msg = format!("Error guardando perfil: {e}"),
            }
        } else {
            self.msg = format!("CSV guardado en {} (perfil necesita ≥2 cargas)", self.csv_path);
        }
    }

    /// Mueve la selección: entre series si el foco está en la serie, o entre reps si está en una rep.
    fn history_move(&mut self, delta: i32) {
        let ids = self.finalized_set_ids();
        if ids.is_empty() {
            return;
        }
        match self.hist_sel_rep {
            Some(rp) => {
                let n = self.reps_of_set(ids[self.hist_sel_set.min(ids.len() - 1)]).len();
                if n > 0 {
                    let new = (rp as i32 + delta).rem_euclid(n as i32) as usize;
                    self.hist_sel_rep = Some(new);
                }
            }
            None => {
                let new = (self.hist_sel_set as i32 + delta).rem_euclid(ids.len() as i32) as usize;
                self.hist_sel_set = new;
            }
        }
    }

    /// Entra a seleccionar reps dentro de la serie resaltada.
    fn history_enter_reps(&mut self) {
        let ids = self.finalized_set_ids();
        if ids.is_empty() {
            return;
        }
        self.hist_sel_set = self.hist_sel_set.min(ids.len() - 1);
        if !self.reps_of_set(ids[self.hist_sel_set]).is_empty() {
            self.hist_sel_rep = Some(self.hist_sel_rep.unwrap_or(0));
        }
    }

    fn history_delete_series(&mut self) {
        let ids = self.finalized_set_ids();
        if let Some(&set_idx) = ids.get(self.hist_sel_set) {
            self.delete_series(set_idx);
            self.hist_sel_rep = None;
            self.clamp_history_selection();
        }
    }

    fn history_delete_rep(&mut self) {
        let ids = self.finalized_set_ids();
        if let (Some(&set_idx), Some(pos)) = (ids.get(self.hist_sel_set), self.hist_sel_rep) {
            self.delete_rep(set_idx, pos);
            self.clamp_history_selection();
        }
    }

    /// Cierra la conexión y vuelve a la pantalla de selección (conserva perfil y CSV).
    fn disconnect_to_select(&mut self) {
        self.rx = None;
        self.current_set.clear();
        self.last_rep = None;
        self.countdown_until = None;
        self.scanning = false;
        self.scan_rx = None;
        self.scan_results.clear();
        self.status = "elige un encoder".into();
        self.msg = "Desconectado. Elige encoder: 'w' WiFi · 'e' escanear BLE · 'q' salir".into();
    }
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

struct Args {
    host: String,
    port: u16,
    exercise: String,
    v1rm: f64,
    load: f64,
    bodyweight: f64,
    added_load: f64,
    rest: f64,
    prep: f64,
    csv: Option<String>,
    profile_path: Option<String>,
    loaded: Option<Lvp>,
}

fn parse_args() -> Args {
    let mut host = ENCODER_HOST.to_string();
    let mut port = ENCODER_PORT;
    let mut exercise = "sentadilla".to_string();
    let mut load = 20.0;
    let mut bodyweight = 75.0;
    let mut added_load = 0.0;
    let mut rest = 20.0;
    let mut prep = 5.0;
    let mut csv = None;
    let mut profile_path = None;
    let mut loaded = None;
    let mut v1rm_override: Option<f64> = None;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--host" => host = it.next().unwrap_or(host),
            "--port" => port = it.next().and_then(|v| v.parse().ok()).unwrap_or(port),
            "--exercise" => exercise = it.next().unwrap_or(exercise),
            "--load" => load = it.next().and_then(|v| v.parse().ok()).unwrap_or(load),
            "--bodyweight" => bodyweight = it.next().and_then(|v| v.parse().ok()).unwrap_or(bodyweight),
            "--added-load" | "--lastre" => added_load = it.next().and_then(|v| v.parse().ok()).unwrap_or(added_load),
            "--rest" => rest = it.next().and_then(|v| v.parse().ok()).unwrap_or(rest),
            "--prep" => prep = it.next().and_then(|v| v.parse().ok()).unwrap_or(prep),
            "--v1rm" => v1rm_override = it.next().and_then(|v| v.parse().ok()),
            "--csv" => csv = it.next(),
            "--profile" => {
                if let Some(p) = it.next() {
                    if let Ok(l) = openspd_io::load_profile(&p) {
                        exercise = l.exercise.clone();
                        loaded = Some(l);
                    }
                    profile_path = Some(p);
                }
            }
            _ => {}
        }
    }
    let v1rm = v1rm_override.unwrap_or_else(|| default_v1rm(&exercise));
    Args { host, port, exercise, v1rm, load, bodyweight, added_load, rest, prep, csv, profile_path, loaded }
}

fn main() -> io::Result<()> {
    let args = parse_args();
    let csv_path = args.csv.unwrap_or_else(|| format!("sesion_{}.csv", now_unix()));
    let profile_path = args.profile_path.unwrap_or_else(|| format!("{}.lvp", args.exercise));

    let (points, lvp) = match args.loaded {
        Some(l) => (l.points.clone(), Some(l)),
        None => (Vec::new(), None),
    };
    let point_set = vec![None; points.len()]; // los puntos precargados no tienen reps en el log

    let mut app = App {
        exercise: args.exercise,
        v1rm: args.v1rm,
        load: args.load,
        bodyweight_kg: args.bodyweight,
        added_load_kg: args.added_load,
        status: "iniciando…".into(),
        set_idx: 1,
        current_set: Vec::new(),
        log: Vec::new(),
        points,
        point_set,
        lvp,
        hist_sel_set: 0,
        hist_sel_rep: None,
        last_rep: None,
        rest: Duration::from_secs_f64(args.rest),
        prep_secs: args.prep,
        countdown_until: None,
        csv_path,
        profile_path,
        msg: "Elige encoder: 'w' WiFi · 'e' escanear BLE · 'q' salir".into(),
        quit: false,
        rx: None,
        scanning: false,
        scan_rx: None,
        scan_results: Vec::new(),
    };
    let _ = (args.host, args.port); // v1 usa el destino por defecto

    let mut terminal = ratatui::init();
    let res = run(&mut terminal, &mut app);
    ratatui::restore();

    app.save_all();
    println!("{}", app.msg);
    if let Some(lvp) = &app.lvp {
        if lvp.is_valid() {
            println!(
                "Perfil {}: 1RM {:.0} kg · v=a+b·carga con a={:.3} b={:.4} · R² {:.3} · {} cargas",
                lvp.exercise, lvp.one_rm_kg, lvp.intercept, lvp.slope, lvp.r2, lvp.points.len()
            );
        }
    }
    res
}

fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()> {
    while !app.quit {
        // recoger resultados del escaneo BLE
        if let Some(srx) = &app.scan_rx {
            if let Ok(res) = srx.try_recv() {
                app.scanning = false;
                match res {
                    Ok(list) => {
                        app.scan_results = list;
                        app.msg = if app.scan_results.is_empty() {
                            "No se encontraron encoders v2 (¿encendido? ¿BT del móvil apagado?)".into()
                        } else {
                            "Pulsa el número del encoder para conectar.".into()
                        };
                    }
                    Err(e) => app.msg = format!("escaneo: {e}"),
                }
                app.scan_rx = None;
            }
        }

        if app.rx.is_some() {
            // ¿hay una cuenta atrás de preparación activa? si acaba de expirar, la cerramos
            let counting_down = match app.countdown_until {
                Some(end) if Instant::now() < end => true,
                Some(_) => {
                    app.countdown_until = None;
                    app.msg = "¡Ya! Empieza la serie.".into();
                    false
                }
                None => false,
            };

            // drenar mensajes (recoger primero para evitar conflicto de borrow)
            let mut incoming = Vec::new();
            if let Some(rx) = &app.rx {
                while let Ok(m) = rx.try_recv() {
                    incoming.push(m);
                }
            }
            for m in incoming {
                match m {
                    // durante la cuenta atrás descartamos las reps: te estás colocando en posición
                    RepEvent::Rep(rep) => {
                        if !counting_down {
                            app.add_rep(rep);
                        }
                    }
                    RepEvent::Status(s) => app.status = s,
                    RepEvent::Closed => app.status = "conexión cerrada por el encoder".into(),
                }
            }
            // fin de serie por descanso (no mientras nos preparamos)
            if !counting_down {
                if let Some(t) = app.last_rep {
                    if !app.current_set.is_empty() && t.elapsed() >= app.rest {
                        app.finalize_set();
                    }
                }
            }
        }

        if app.rx.is_some() {
            terminal.draw(|f| ui(f, app))?;
        } else {
            terminal.draw(|f| ui_select(f, app))?;
        }

        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                if app.rx.is_some() {
                    // modo conectado
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                        KeyCode::Char('b') => app.disconnect_to_select(),
                        // en peso corporal +/- [ ] editan el lastre (puede ser negativo); si no, la carga
                        KeyCode::Char('+') | KeyCode::Char('=') => app.adjust_load(2.5),
                        KeyCode::Char('-') | KeyCode::Char('_') => app.adjust_load(-2.5),
                        KeyCode::Char(']') => app.adjust_load(10.0),
                        KeyCode::Char('[') => app.adjust_load(-10.0),
                        KeyCode::Char('c') => app.finalize_set(),
                        KeyCode::Char('t') => app.start_countdown(),
                        KeyCode::Char('<') | KeyCode::Char(',') => {
                            app.prep_secs = (app.prep_secs - 1.0).max(0.0);
                            app.msg = format!("Preparación: {:.0} s", app.prep_secs);
                        }
                        KeyCode::Char('>') | KeyCode::Char('.') => {
                            app.prep_secs = (app.prep_secs + 1.0).min(60.0);
                            app.msg = format!("Preparación: {:.0} s", app.prep_secs);
                        }
                        KeyCode::Char('s') => app.save_all(),
                        KeyCode::Char('u') => {
                            if app.points.pop().is_some() {
                                app.point_set.pop();
                                app.refit();
                                app.msg = "Último punto del perfil eliminado".into();
                            }
                        }
                        // navegación y borrado de series anteriores (revisión en línea)
                        KeyCode::Up | KeyCode::Char('k') => app.history_move(-1),
                        KeyCode::Down | KeyCode::Char('j') => app.history_move(1),
                        KeyCode::Right | KeyCode::Enter => app.history_enter_reps(),
                        KeyCode::Left => app.hist_sel_rep = None,
                        KeyCode::Char('d') => app.history_delete_series(),
                        KeyCode::Char('x') => app.history_delete_rep(),
                        _ => {}
                    }
                } else {
                    // modo selección de encoder
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                        KeyCode::Char('w') => {
                            app.status = "conectando (v1 WiFi)…".into();
                            app.rx = Some(openspd_io::spawn_tcp_reader(ENCODER_HOST.to_string(), ENCODER_PORT));
                        }
                        KeyCode::Char('e') if !app.scanning => {
                            app.scanning = true;
                            app.scan_results.clear();
                            app.msg = "buscando encoders v2 (≈6 s)…".into();
                            app.scan_rx = Some(openspd_io::spawn_ble_scan(openspd_io::DEFAULT_SCAN_SECS));
                        }
                        KeyCode::Char(d) if d.is_ascii_digit() => {
                            let n = d.to_digit(10).unwrap() as usize;
                            if n >= 1 && n <= app.scan_results.len() {
                                let addr = app.scan_results[n - 1].0.clone();
                                app.status = format!("conectando (v2) a {addr}…");
                                app.rx = Some(openspd_io::spawn_ble_reader(addr));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

fn ui_select(f: &mut Frame, app: &App) {
    let root = Layout::vertical([Constraint::Length(3), Constraint::Min(6), Constraint::Length(3)])
        .split(f.area());

    let title = Line::from(vec![
        Span::styled(" OpenSPD ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  Elige el encoder"),
    ]);
    f.render_widget(Paragraph::new(title).block(Block::default().borders(Borders::ALL)), root[0]);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" w ", Style::default().fg(Color::Black).bg(Color::Green)),
        Span::raw("  Encoder v1 — WiFi/TCP  ("),
        Span::raw(format!("{ENCODER_HOST}:{ENCODER_PORT}")),
        Span::raw(")"),
    ]));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled(" e ", Style::default().fg(Color::Black).bg(Color::Green)),
        Span::raw("  Encoder v2 — BLE: escanear"),
        Span::styled(
            if app.scanning { "   (buscando…)" } else { "" },
            Style::default().fg(Color::Yellow),
        ),
    ]));
    for (i, (_, label)) in app.scan_results.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", i + 1), Style::default().fg(Color::Black).bg(Color::Gray)),
            Span::raw(format!("  {label}")),
        ]));
    }
    if !app.msg.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::styled(app.msg.clone(), Style::default().fg(Color::DarkGray)));
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Encoders ")),
        root[1],
    );

    let help = Line::from(vec![
        Span::styled(" w ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" WiFi  "),
        Span::styled(" e ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" escanear BLE  "),
        Span::styled(" 1-9 ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" conectar  "),
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" salir"),
    ]);
    f.render_widget(Paragraph::new(help).block(Block::default().borders(Borders::ALL).title(" Atajos ")), root[2]);
}

fn ui(f: &mut Frame, app: &App) {
    let root = Layout::vertical([
        Constraint::Length(3), // status
        Constraint::Min(8),    // cuerpo
        Constraint::Length(3), // ayuda
    ])
    .split(f.area());

    draw_status(f, root[0], app);

    let body = Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(root[1]);

    draw_current_set(f, body[0], app);
    draw_profile(f, body[1], app);
    draw_help(f, root[2], app);
}

/// Etiqueta de carga para la barra de estado: en peso corporal desglosa peso corporal + lastre.
fn load_label(app: &App) -> String {
    if profile::is_bodyweight(&app.exercise) {
        format!(
            "PC {:.1} + lastre {:.1} = {:.1} kg",
            app.bodyweight_kg,
            app.added_load_kg,
            app.current_load()
        )
    } else {
        format!("{:.1} kg", app.current_load())
    }
}

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    // si hay cuenta atrás de preparación activa, la mostramos bien visible
    if let Some(rem) = app.countdown_remaining() {
        let line = Line::from(vec![
            Span::styled(
                format!(" ⏱ PREPÁRATE · EMPIEZA EN {:.0} ", rem.ceil()),
                Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   Carga: "),
            Span::styled(load_label(app), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("   (las repeticiones no cuentan todavía)"),
        ]);
        f.render_widget(Paragraph::new(line).block(Block::default().borders(Borders::ALL)), area);
        return;
    }
    let line = Line::from(vec![
        Span::styled(" OpenSPD ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  Ejercicio: "),
        Span::styled(&app.exercise, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("   Carga: "),
        Span::styled(load_label(app), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw("   Serie: "),
        Span::styled(format!("{}", app.set_idx), Style::default().fg(Color::Magenta)),
        Span::raw("   ["),
        Span::styled(&app.status, Style::default().fg(Color::White)),
        Span::raw("]"),
    ]);
    f.render_widget(Paragraph::new(line).block(Block::default().borders(Borders::ALL)), area);
}

fn draw_current_set(f: &mut Frame, area: Rect, app: &App) {
    let has_history = !app.finalized_set_ids().is_empty();
    let chunks = if has_history {
        Layout::vertical([Constraint::Min(4), Constraint::Length(10), Constraint::Length(3)]).split(area)
    } else {
        Layout::vertical([Constraint::Min(4), Constraint::Length(3)]).split(area)
    };
    let gauge_area = chunks[chunks.len() - 1];

    let rows: Vec<Row> = app
        .current_set
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let vl = velocity_loss(&app.current_set[..=i]);
            let vstyle = if r.mean_velocity >= 0.75 {
                Style::default().fg(Color::Green)
            } else if r.mean_velocity >= 0.5 {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Red)
            };
            Row::new(vec![
                Cell::from(format!("{}", r.rep)),
                Cell::from(format!("{:.2}", r.mean_velocity)).style(vstyle),
                Cell::from(format!("{:.1}", r.rom)),
                Cell::from(format!("{:.2}", r.peak_velocity)),
                Cell::from(format!("{:.1}%", vl)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
        ],
    )
    .header(
        Row::new(vec!["rep", "media", "ROM", "pico", "VL%"])
            .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Serie actual ({} reps) ", app.current_set.len())),
    );
    f.render_widget(table, chunks[0]);

    // revisión en línea de series anteriores (si hay alguna cerrada)
    if has_history {
        draw_history(f, chunks[1], app);
    }

    // gauge de velocity loss
    let vl = velocity_loss(&app.current_set);
    let ratio = (vl / 40.0).clamp(0.0, 1.0);
    let color = if vl >= 30.0 {
        Color::Red
    } else if vl >= 20.0 {
        Color::Yellow
    } else {
        Color::Green
    };
    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" Velocity Loss (serie) "))
        .gauge_style(Style::default().fg(color))
        .ratio(ratio)
        .label(format!("{vl:.1} %"));
    f.render_widget(gauge, gauge_area);
}

/// Revisión en línea de las series ya cerradas: lista de series y, al entrar (→), sus reps.
/// La serie/rep resaltada se borra con `d`/`x`.
fn draw_history(f: &mut Frame, area: Rect, app: &App) {
    let ids = app.finalized_set_ids();
    let sel = app.hist_sel_set.min(ids.len().saturating_sub(1));
    let on_reps = app.hist_sel_rep.is_some();

    // si el foco está en una rep, mostramos las reps de la serie seleccionada; si no, la lista de series
    let rows: Vec<Row> = if on_reps {
        let set_idx = ids[sel];
        let sel_rep = app.hist_sel_rep.unwrap_or(0);
        app.reps_of_set(set_idx)
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let mark = if i == sel_rep { "▶" } else { " " };
                let row = Row::new(vec![
                    Cell::from(format!("{mark} {}", i + 1)),
                    Cell::from(format!("{:.2}", r.mean_velocity)),
                    Cell::from(format!("{:.1}", r.rom)),
                    Cell::from(format!("{:.2}", r.peak_velocity)),
                ]);
                if i == sel_rep {
                    row.style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    row
                }
            })
            .collect()
    } else {
        ids.iter()
            .enumerate()
            .map(|(i, &set_idx)| {
                let reps = app.reps_of_set(set_idx);
                let best = reps.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
                let load = app
                    .point_set
                    .iter()
                    .position(|p| *p == Some(set_idx))
                    .map(|pi| app.points[pi].load_kg);
                let mark = if i == sel { "▶" } else { " " };
                let row = Row::new(vec![
                    Cell::from(format!("{mark} {set_idx}")),
                    Cell::from(load.map(|l| format!("{l:.1}")).unwrap_or_else(|| "—".into())),
                    Cell::from(format!("{}", reps.len())),
                    Cell::from(format!("{best:.2}")),
                ]);
                if i == sel {
                    row.style(Style::default().add_modifier(Modifier::REVERSED))
                } else {
                    row
                }
            })
            .collect()
    };

    let (header, widths, title) = if on_reps {
        (
            vec!["rep", "media", "ROM", "pico"],
            [Constraint::Length(6), Constraint::Length(8), Constraint::Length(8), Constraint::Length(8)],
            format!(" Reps serie {}  (x borra rep · ← vuelve) ", ids[sel]),
        )
    } else {
        (
            vec!["serie", "carga", "reps", "mejor V"],
            [Constraint::Length(6), Constraint::Length(8), Constraint::Length(8), Constraint::Length(8)],
            " Series anteriores  (↑↓ mover · → reps · d borra serie) ".to_string(),
        )
    };

    let table = Table::new(rows, widths)
        .header(Row::new(header).style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)))
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(table, area);
}

fn draw_profile(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([Constraint::Min(4), Constraint::Length(9)]).split(area);

    // tabla de puntos del perfil
    let rows: Vec<Row> = app
        .points
        .iter()
        .map(|p| {
            let pct = app
                .lvp
                .as_ref()
                .map(|l| format!("{:.0}%", l.pct_1rm(p.best_velocity)))
                .unwrap_or_else(|| "—".into());
            Row::new(vec![
                Cell::from(format!("{:.1}", p.load_kg)),
                Cell::from(format!("{:.2}", p.best_velocity)),
                Cell::from(pct),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [Constraint::Length(10), Constraint::Length(10), Constraint::Length(8)],
    )
    .header(
        Row::new(vec!["carga", "mejor V", "%1RM"])
            .style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
    )
    .block(Block::default().borders(Borders::ALL).title(" Puntos del perfil "));
    f.render_widget(table, chunks[0]);

    // resumen del ajuste
    let mut lines: Vec<Line> = Vec::new();
    match &app.lvp {
        Some(l) if l.is_valid() => {
            lines.push(Line::from(vec![
                Span::raw("1RM estimado: "),
                Span::styled(format!("{:.0} kg", l.one_rm_kg), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            ]));
            lines.push(Line::raw(format!("recta: v = {:.3} {:+.4}·carga", l.intercept, l.slope)));
            lines.push(Line::raw(format!("R² = {:.3}   ·   V1RM = {:.2} m/s", l.r2, l.v1rm)));
            lines.push(Line::raw("velocidad objetivo por %1RM:"));
            let targets = [100.0, 90.0, 80.0, 70.0, 60.0];
            let s: Vec<String> = targets.iter().map(|p| format!("{:.0}%→{:.2}", p, l.velocity_for_pct(*p))).collect();
            lines.push(Line::styled(s.join("  "), Style::default().fg(Color::Yellow)));
        }
        Some(_) => lines.push(Line::styled(
            "perfil incoherente (la velocidad debería bajar al subir la carga)",
            Style::default().fg(Color::Red),
        )),
        None => lines.push(Line::styled(
            "añade ≥2 cargas distintas para ajustar el perfil",
            Style::default().fg(Color::DarkGray),
        )),
    }
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Perfil carga-velocidad ")),
        chunks[1],
    );
}

fn draw_help(f: &mut Frame, area: Rect, app: &App) {
    let help = Line::from(vec![
        Span::styled(" +/- ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(if profile::is_bodyweight(&app.exercise) { " lastre ±2.5  " } else { " carga ±2.5  " }),
        Span::styled(" [ ] ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" ±10  "),
        Span::styled(" c ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" cerrar serie  "),
        Span::styled(" t ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" preparar (cuenta atrás)  "),
        Span::styled(" <  > ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" ±1 s prep  "),
        Span::styled(" ↑↓→← ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" revisar series  "),
        Span::styled(" d ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" borrar serie  "),
        Span::styled(" x ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" borrar rep  "),
        Span::styled(" u ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" deshacer punto  "),
        Span::styled(" s ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" guardar  "),
        Span::styled(" b ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" cambiar encoder  "),
        Span::styled(" q ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" salir"),
    ]);
    let block = Block::default().borders(Borders::ALL).title(" Atajos ");
    let inner = Line::from(app.msg.clone()).style(Style::default().fg(Color::White));
    let para = Paragraph::new(vec![help, inner]).block(block);
    f.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(n: u32, v: f64) -> Rep {
        Rep { rep: n, mean_velocity: v, rom: 50.0, peak_velocity: v + 0.3 }
    }

    /// App de prueba con `n` series cerradas (set_idx 1..=n) y la siguiente serie en curso vacía.
    /// `sets[i]` son las velocidades medias de las reps de la serie i+1.
    fn app_with(sets: &[(f64, Vec<f64>)]) -> App {
        let mut log = Vec::new();
        let mut points = Vec::new();
        let mut point_set = Vec::new();
        for (si, (load, vels)) in sets.iter().enumerate() {
            let set_idx = si as u32 + 1;
            for (ri, v) in vels.iter().enumerate() {
                log.push((set_idx, rep(ri as u32 + 1, *v), 1000 + ri as u64));
            }
            let best = vels.iter().cloned().fold(f64::MIN, f64::max);
            points.push(Point { load_kg: *load, best_velocity: best });
            point_set.push(Some(set_idx));
        }
        let tmp = std::env::temp_dir().join(format!("openspd_test_{}.csv", points.len()));
        App {
            exercise: "sentadilla".into(),
            v1rm: 0.3,
            load: 20.0,
            bodyweight_kg: 75.0,
            added_load_kg: 0.0,
            status: String::new(),
            set_idx: sets.len() as u32 + 1,
            current_set: Vec::new(),
            log,
            points,
            point_set,
            lvp: None,
            hist_sel_set: 0,
            hist_sel_rep: None,
            last_rep: None,
            rest: Duration::from_secs(20),
            prep_secs: 5.0,
            countdown_until: None,
            csv_path: tmp.to_string_lossy().into_owned(),
            profile_path: "test.lvp".into(),
            msg: String::new(),
            quit: false,
            rx: None,
            scanning: false,
            scan_rx: None,
            scan_results: Vec::new(),
        }
    }

    #[test]
    fn delete_rep_recomputes_best_velocity() {
        let mut app = app_with(&[(40.0, vec![1.0, 0.9, 0.8]), (60.0, vec![0.85, 0.8])]);
        // quita la mejor rep de la serie 1 (1.0); la nueva mejor debe ser 0.9
        app.delete_rep(1, 0);
        assert_eq!(app.reps_of_set(1).len(), 2);
        let i = app.point_set.iter().position(|p| *p == Some(1)).unwrap();
        assert!((app.points[i].best_velocity - 0.9).abs() < 1e-9);
        assert_eq!(app.set_idx, 3); // no se borró ninguna serie
    }

    #[test]
    fn delete_last_rep_removes_series_and_renumbers() {
        let mut app = app_with(&[(40.0, vec![1.0]), (60.0, vec![0.85, 0.8]), (80.0, vec![0.7])]);
        // la serie 1 tiene una sola rep: borrarla elimina la serie y renumera
        app.delete_rep(1, 0);
        let ids = app.finalized_set_ids();
        assert_eq!(ids, vec![1, 2]); // antes 2 y 3, ahora renumeradas a 1 y 2
        assert_eq!(app.points.len(), 2);
        // la serie ahora-1 era la antigua serie 2 (0.85, 0.80)
        assert!((app.reps_of_set(1).iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max) - 0.85).abs() < 1e-9);
        assert_eq!(app.set_idx, 3); // 4 -> 3
    }

    #[test]
    fn delete_series_renumbers_following() {
        let mut app = app_with(&[(40.0, vec![1.0]), (60.0, vec![0.85]), (80.0, vec![0.7])]);
        app.delete_series(2);
        let ids = app.finalized_set_ids();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(app.point_set, vec![Some(1), Some(2)]);
        // la serie 2 (renumerada) es la antigua serie 3, carga 80
        let i = app.point_set.iter().position(|p| *p == Some(2)).unwrap();
        assert!((app.points[i].load_kg - 80.0).abs() < 1e-9);
        assert_eq!(app.set_idx, 3);
    }

    #[test]
    fn preloaded_points_are_preserved() {
        let mut app = app_with(&[(40.0, vec![1.0]), (60.0, vec![0.85])]);
        // simula un perfil precargado: un punto extra sin reps al principio
        app.points.insert(0, Point { load_kg: 100.0, best_velocity: 0.5 });
        app.point_set.insert(0, None);
        app.delete_series(1);
        // el punto precargado (None) sigue ahí; sólo se renumeran las series de sesión
        assert!(app.point_set.contains(&None));
        assert!(app.points.iter().any(|p| (p.load_kg - 100.0).abs() < 1e-9));
        assert_eq!(app.finalized_set_ids(), vec![1]); // la antigua serie 2 pasa a 1
        assert_eq!(app.point_set, vec![None, Some(1)]);
    }
}
