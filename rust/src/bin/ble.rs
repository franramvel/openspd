// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! OpenSPD — cliente BLE para el encoder VBT "v2" (datos cifrados).
//!
//! Escanea los encoders disponibles, te deja ELEGIR uno, conecta, descifra las repeticiones
//! (AES-128-ECB) y las muestra/guarda con TODO el detalle (ambas fases, aceleraciones). Todo el
//! transporte BLE vive en `openspd-io`; este binario es solo CLI + presentación.
//!
//! Uso:
//!   openspd-ble                      escanea y muestra un selector
//!   openspd-ble --address AA:BB:..   conecta directo a esa MAC
//!   openspd-ble --scan-secs 8        tiempo de escaneo (def. 6)
//!   openspd-ble --no-begin           no enviar comando de inicio de serie
//!   openspd-ble --csv sesion.csv

use std::io::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use openspd_core::encoderv2::{EncoderV2Rep, Phase};
use openspd_core::metrics::{load_zone, velocity_loss};
use openspd_core::protocol::Rep;
use openspd_io::{BleEvent, DEFAULT_SCAN_SECS};

struct Args {
    address: Option<String>,
    scan_secs: u64,
    send_begin: bool,
    csv: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args { address: None, scan_secs: DEFAULT_SCAN_SECS, send_begin: true, csv: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--address" => a.address = it.next(),
            "--scan-secs" => a.scan_secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_SCAN_SECS),
            "--no-begin" => a.send_begin = false,
            "--csv" => a.csv = it.next(),
            "-h" | "--help" => {
                println!("Uso: openspd-ble [--address MAC] [--scan-secs N] [--no-begin] [--csv ARCHIVO]");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    a
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Presenta los encoders encontrados y deja elegir uno por stdin. Devuelve la dirección elegida.
fn choose(cands: &[(String, String)]) -> Option<String> {
    println!("\nEncoders disponibles:");
    for (i, (_, label)) in cands.iter().enumerate() {
        println!("  [{}] {}", i + 1, label);
    }
    print!("Elige un encoder (número, Enter=1): ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    let line = line.trim();
    let idx = if line.is_empty() {
        0
    } else {
        line.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= cands.len())? - 1
    };
    Some(cands[idx].0.clone())
}

fn main() {
    eprintln!(
        "OpenSPD {} · GPL-3.0-or-later, sin garantía. Software NO oficial; cliente de interoperabilidad.\n\
         Estimaciones (%1RM, velocity loss) orientativas: NO sustituyen a un profesional; \
         entrenas bajo tu propia responsabilidad.",
        env!("CARGO_PKG_VERSION")
    );
    let args = parse_args();
    let csv_path = args.csv.clone().unwrap_or_else(|| format!("sesion_ble_{}.csv", now_unix()));

    // Elegir dirección: directa (--address) o por escaneo + selector.
    let address = match args.address.clone() {
        Some(a) => a,
        None => {
            println!("Escaneando {}s…", args.scan_secs);
            let scan = openspd_io::spawn_ble_scan(args.scan_secs);
            let cands = match scan.recv() {
                Ok(Ok(list)) => list,
                Ok(Err(e)) => {
                    eprintln!("Error de escaneo: {e}");
                    std::process::exit(1);
                }
                Err(_) => {
                    eprintln!("Escaneo interrumpido.");
                    std::process::exit(1);
                }
            };
            if cands.is_empty() {
                eprintln!("No se encontraron encoders. Enciende el encoder y reintenta.");
                std::process::exit(1);
            }
            match choose(&cands) {
                Some(a) => a,
                None => {
                    eprintln!("Selección inválida.");
                    std::process::exit(1);
                }
            }
        }
    };

    println!("\nGuardando en: {csv_path}");
    println!("{:>4} {:>4} | {:>6} | {:>7} | {:>6} | {:>5} | zona", "REP", "FASE", "MPV", "ROM", "PICO", "VL%");
    println!("{}", "-".repeat(72));

    // Conexión y lectura completa (ambas fases) en openspd-io; aquí solo presentamos y guardamos.
    let rx = openspd_io::spawn_ble_reader_full(address, args.send_begin);
    let mut log: Vec<(EncoderV2Rep, u64)> = Vec::new();
    let mut concentric: Vec<Rep> = Vec::new();

    while let Ok(ev) = rx.recv() {
        match ev {
            BleEvent::Status(s) => eprintln!("[{s}]"),
            BleEvent::Rep(rep) => {
                log.push((rep, now_unix()));
                let fase = if rep.phase == Phase::Concentric { "C" } else { "E" };
                let vl = if rep.phase == Phase::Concentric {
                    concentric.push(rep.to_rep());
                    velocity_loss(&concentric)
                } else {
                    0.0
                };
                println!(
                    "{:>4} {:>4} | {:>6.2} | {:>7.2} | {:>6.2} | {:>4.1}% | {}",
                    rep.rep, fase, rep.mpv, rep.rom, rep.peak_velocity, vl, load_zone(rep.mpv)
                );
                let _ = openspd_io::save_ble_csv(&csv_path, &log);
            }
            BleEvent::Closed => break,
        }
    }

    println!("Reps registradas: {} · CSV: {}", log.len(), csv_path);
}
