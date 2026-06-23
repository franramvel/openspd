// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! App de PC (Rust) para el encoder VBT (1a gen, WiFi).
//!
//! - Conecta por TCP a 192.168.4.1:80.
//! - Decodifica cada repeticion en vivo: velocidad media/pico, ROM, VELOCITY LOSS.
//! - Detecta SERIES automaticamente por el descanso entre reps y muestra un resumen por serie.
//! - Estima %1RM (y 1RM) si le das ejercicio y carga.
//! - Guarda todo en CSV de forma INCREMENTAL (tras cada rep): un Ctrl-C nunca pierde datos.
//!
//! Uso:
//!   openspd                                   graba una sesion (Ctrl-C para terminar)
//!   openspd --exercise banca --load 80        estima %1RM y 1RM con esa carga
//!   openspd --vl-stop 20                      avisa al superar 20% de velocity loss en la serie
//!   openspd --rest 25                         considera nueva serie tras 25s sin reps (def. 30)
//!   openspd --csv sesion.csv                  archivo de salida
//!
//! Ejercicios con ecuacion de %1RM: banca/bench/press, sentadilla/squat.

use std::fs::File;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openspd::metrics::{est_1rm_kg, est_1rm_pct, load_zone, summarize, velocity_loss};
use openspd::profile::{self, Lvp};
use openspd::protocol::{parse_line, Rep, ENCODER_HOST, ENCODER_PORT};

struct Args {
    host: String,
    port: u16,
    vl_stop: Option<f64>,
    csv: Option<String>,
    exercise: Option<String>,
    load: Option<f64>,
    rest: f64, // segundos sin reps para considerar nueva serie
    profile: Option<Lvp>,
}

/// Rep + numero de serie + timestamp unix, para el registro/CSV.
struct LoggedRep {
    set: u32,
    rep: Rep,
    t_unix: u64,
}

fn parse_args() -> Args {
    let mut a = Args {
        host: ENCODER_HOST.to_string(),
        port: ENCODER_PORT,
        vl_stop: None,
        csv: None,
        exercise: None,
        load: None,
        rest: 30.0,
        profile: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--host" => a.host = it.next().unwrap_or_default(),
            "--port" => a.port = it.next().and_then(|v| v.parse().ok()).unwrap_or(ENCODER_PORT),
            "--vl-stop" => a.vl_stop = it.next().and_then(|v| v.parse().ok()),
            "--csv" => a.csv = it.next(),
            "--exercise" => a.exercise = it.next(),
            "--load" => a.load = it.next().and_then(|v| v.parse().ok()),
            "--rest" => a.rest = it.next().and_then(|v| v.parse().ok()).unwrap_or(30.0),
            "--profile" => {
                if let Some(path) = it.next() {
                    match profile::load(&path) {
                        Ok(p) => {
                            eprintln!("Perfil cargado: {} · 1RM {:.0} kg · R² {:.3}", p.exercise, p.one_rm_kg, p.r2);
                            a.profile = Some(p);
                        }
                        Err(e) => eprintln!("(aviso) no se pudo cargar perfil: {e}"),
                    }
                }
            }
            "-h" | "--help" => {
                println!("Uso: openspd [--exercise NOMBRE] [--load KG] [--vl-stop PCT] [--rest SEG] [--csv ARCHIVO] [--host H] [--port P]");
                std::process::exit(0);
            }
            other => eprintln!("(aviso) argumento ignorado: {other}"),
        }
    }
    a
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn save_csv(path: &str, log: &[LoggedRep]) -> std::io::Result<()> {
    let mut f = File::create(path)?;
    writeln!(f, "set,rep,vel_media_mps,rom_cm,vel_maxima_mps,t_unix")?;
    for l in log {
        writeln!(
            f,
            "{},{},{},{},{},{}",
            l.set, l.rep.rep, l.rep.mean_velocity, l.rep.rom, l.rep.peak_velocity, l.t_unix
        )?;
    }
    Ok(())
}

/// Imprime el resumen de una serie + estimacion de %1RM/1RM si procede.
fn print_set_summary(set: u32, reps: &[Rep], args: &Args) {
    if let Some(s) = summarize(reps) {
        println!("\n  ▌ SERIE {set} terminada");
        for line in s.to_string().lines() {
            println!("  {line}");
        }
        if let Some(lvp) = &args.profile {
            // perfil individual: mas preciso
            let pct = lvp.pct_1rm(s.best_mean_velocity);
            println!(
                "  → %1RM {:.0}% · 1RM {:.0} kg  [tu perfil {}, R² {:.3}]",
                pct, lvp.one_rm_kg, lvp.exercise, lvp.r2
            );
        } else if let Some(ex) = &args.exercise {
            // ecuacion poblacional
            if let Some(pct) = est_1rm_pct(ex, s.best_mean_velocity) {
                print!("  → %1RM estimado ({ex}): {pct:.0}%");
                if let Some(load) = args.load {
                    if let Some(rm) = est_1rm_kg(load, pct) {
                        print!("  ·  1RM ≈ {rm:.0} kg (con {load:.0} kg)");
                    }
                }
                println!("  [aproximado/poblacional]");
            }
        }
        println!();
    }
}

fn main() {
    eprintln!(
        "OpenSPD {} · GPL-3.0-or-later, sin garantía. Software NO oficial; sin relación con \
         Speed4lifts ni Vitruve (ver DISCLAIMER).\n\
         Estimaciones (%1RM, velocity loss) orientativas: NO sustituyen a un profesional; \
         entrenas bajo tu propia responsabilidad.",
        env!("CARGO_PKG_VERSION")
    );
    let args = parse_args();
    let csv_path = args
        .csv
        .clone()
        .unwrap_or_else(|| format!("sesion_{}.csv", now_unix()));

    println!(
        "Conectando a {}:{} ... (Ctrl-C para terminar)",
        args.host, args.port
    );
    let mut stream = match TcpStream::connect((args.host.as_str(), args.port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error de red: {e}  (¿WiFi conectado al encoder?)");
            std::process::exit(1);
        }
    };
    // timeout corto para poder detectar el fin de serie aunque no lleguen datos
    let _ = stream.set_read_timeout(Some(Duration::from_millis(400)));
    println!("Conectado. Guardando en: {csv_path}");
    if let Some(ex) = &args.exercise {
        println!("Ejercicio: {ex}{}", args.load.map(|l| format!(" · carga {l:.0} kg")).unwrap_or_default());
    }
    println!("Nueva serie tras {:.0}s de descanso. Empieza a entrenar.\n", args.rest);
    println!(
        "{:>3} {:>4} | {:>6} | {:>7} | {:>6} | {:>5} | zona",
        "SET", "REP", "MEDIA", "ROM", "PICO", "VL%"
    );
    println!("{}", "-".repeat(74));

    let mut log: Vec<LoggedRep> = Vec::new();
    let mut current_set: Vec<Rep> = Vec::new();
    let mut set_idx: u32 = 1;
    let mut last_rep_at: Option<Instant> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 1024];
    let rest = Duration::from_secs_f64(args.rest);

    loop {
        match stream.read(&mut tmp) {
            Ok(0) => {
                println!("\n[el encoder cerro la conexion]");
                break;
            }
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                // procesar lineas completas (terminadas en \n; el \r se ignora al parsear)
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw);
                    if let Some(rep) = parse_line(&line) {
                        current_set.push(rep);
                        last_rep_at = Some(Instant::now());
                        let vl = velocity_loss(&current_set);
                        println!(
                            "{:>3} {:>4} | {:>6.2} | {:>7.2} | {:>6.2} | {:>4.1}% | {}",
                            set_idx, rep.rep, rep.mean_velocity, rep.rom, rep.peak_velocity, vl,
                            load_zone(rep.mean_velocity)
                        );
                        log.push(LoggedRep { set: set_idx, rep, t_unix: now_unix() });
                        if let Err(e) = save_csv(&csv_path, &log) {
                            eprintln!("(aviso) no se pudo guardar CSV: {e}");
                        }
                        if let Some(limit) = args.vl_stop {
                            if vl >= limit {
                                println!("  ⚠️  VELOCITY LOSS {vl:.1}% ≥ {limit:.0}% → considera parar la serie");
                            }
                        }
                    } else if !line.trim().is_empty() {
                        println!("[?] linea no reconocida: {:?}", line.trim());
                    }
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => {
                eprintln!("\n[error de lectura: {e}]");
                break;
            }
        }

        // fin de serie por descanso
        if !current_set.is_empty() {
            if let Some(t) = last_rep_at {
                if t.elapsed() >= rest {
                    print_set_summary(set_idx, &current_set, &args);
                    set_idx += 1;
                    current_set.clear();
                    last_rep_at = None;
                }
            }
        }
    }

    // cerrar la ultima serie si quedo abierta
    if !current_set.is_empty() {
        print_set_summary(set_idx, &current_set, &args);
    }

    let all: Vec<Rep> = log.iter().map(|l| l.rep).collect();
    let n_sets = log.iter().map(|l| l.set).max().unwrap_or(0);
    println!("══ SESION: {} series · {} reps ══", n_sets, all.len());
    if !all.is_empty() {
        let best = all.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
        println!("  Mejor velocidad media de la sesion: {best:.2} m/s");
        println!("  Datos en: {csv_path}");
    }
}
