// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! TUI para el encoder VBT: dashboard en vivo + constructor de perfil carga-velocidad.
//!
//! Flujo para construir tu perfil:
//!   1) Ajusta la carga (kg) con +/-  (paso 2.5) o  [ ]  (paso 10).
//!   2) Haz la serie a esa carga. Cada rep aparece en vivo.
//!   3) Descansa (o pulsa 'c'): la serie se cierra y se añade un punto (carga, mejor velocidad)
//!      al perfil. Repite con 2-3 cargas distintas y el perfil se ajusta solo (1RM, R²).
//!   4) 's' guarda perfil + CSV. 'u' deshace el último punto. 'q' sale (guarda).
//!
//! Uso:
//!   openspd-tui --exercise sentadilla --load 40
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
    status: String,
    set_idx: u32,
    current_set: Vec<Rep>,
    log: Vec<(u32, Rep, u64)>,
    points: Vec<Point>,
    lvp: Option<Lvp>,
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

    fn finalize_set(&mut self) {
        if self.current_set.is_empty() {
            return;
        }
        let best = self
            .current_set
            .iter()
            .map(|r| r.mean_velocity)
            .fold(f64::MIN, f64::max);
        self.points.push(Point { load_kg: self.load, best_velocity: best });
        self.refit();
        self.msg = format!(
            "Serie {} cerrada → punto ({:.1} kg, {:.2} m/s)",
            self.set_idx, self.load, best
        );
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
    Args { host, port, exercise, v1rm, load, rest, prep, csv, profile_path, loaded }
}

fn main() -> io::Result<()> {
    let args = parse_args();
    let csv_path = args.csv.unwrap_or_else(|| format!("sesion_{}.csv", now_unix()));
    let profile_path = args.profile_path.unwrap_or_else(|| format!("{}.lvp", args.exercise));

    let (points, lvp) = match args.loaded {
        Some(l) => (l.points.clone(), Some(l)),
        None => (Vec::new(), None),
    };

    let mut app = App {
        exercise: args.exercise,
        v1rm: args.v1rm,
        load: args.load,
        status: "iniciando…".into(),
        set_idx: 1,
        current_set: Vec::new(),
        log: Vec::new(),
        points,
        lvp,
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
                        KeyCode::Char('+') | KeyCode::Char('=') => app.load += 2.5,
                        KeyCode::Char('-') | KeyCode::Char('_') => app.load = (app.load - 2.5).max(0.0),
                        KeyCode::Char(']') => app.load += 10.0,
                        KeyCode::Char('[') => app.load = (app.load - 10.0).max(0.0),
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
                                app.refit();
                                app.msg = "Último punto del perfil eliminado".into();
                            }
                        }
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

fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    // si hay cuenta atrás de preparación activa, la mostramos bien visible
    if let Some(rem) = app.countdown_remaining() {
        let line = Line::from(vec![
            Span::styled(
                format!(" ⏱ PREPÁRATE · EMPIEZA EN {:.0} ", rem.ceil()),
                Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   Carga: "),
            Span::styled(format!("{:.1} kg", app.load), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
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
        Span::styled(format!("{:.1} kg", app.load), Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw("   Serie: "),
        Span::styled(format!("{}", app.set_idx), Style::default().fg(Color::Magenta)),
        Span::raw("   ["),
        Span::styled(&app.status, Style::default().fg(Color::White)),
        Span::raw("]"),
    ]);
    f.render_widget(Paragraph::new(line).block(Block::default().borders(Borders::ALL)), area);
}

fn draw_current_set(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([Constraint::Min(4), Constraint::Length(3)]).split(area);

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
    f.render_widget(gauge, chunks[1]);
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
        Span::raw(" carga ±2.5  "),
        Span::styled(" [ ] ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" ±10  "),
        Span::styled(" c ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" cerrar serie  "),
        Span::styled(" t ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" preparar (cuenta atrás)  "),
        Span::styled(" <  > ", Style::default().fg(Color::Black).bg(Color::Gray)),
        Span::raw(" ±1 s prep  "),
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
