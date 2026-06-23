// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! OpenSPD — cliente BLE para el encoder VBT "v2" (datos cifrados), vía BlueZ (bluer).
//!
//! Escanea los encoders disponibles, te deja ELEGIR uno, conecta, escribe la llave de desbloqueo
//! (en cuanto se resuelven los servicios, antes de que el encoder corte), se suscribe a las
//! repeticiones, las descifra (AES-128-ECB) y las muestra/guarda.
//!
//! Uso:
//!   openspd-ble                      escanea y muestra un selector
//!   openspd-ble --address AA:BB:..   conecta directo a esa MAC
//!   openspd-ble --scan-secs 8        tiempo de escaneo (def. 6)
//!   openspd-ble --no-begin           no enviar comando de inicio de serie
//!   openspd-ble --csv sesion.csv

use std::collections::HashSet;
use std::error::Error;
use std::io::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bluer::{Adapter, Address, Device};
use futures::StreamExt;
use uuid::Uuid;

use openspd::encoderv2::{
    begin_command, generate_key, parse_repetition, EncoderV2Rep, Phase, Reassembler,
    CHAR_BEGIN_END, CHAR_REPETITION, CHAR_UNLOCK, END_SET_COMMAND, SERVICE_UUID,
};
use openspd::metrics::{load_zone, velocity_loss};
use openspd::protocol::Rep;

struct Args {
    address: Option<String>,
    scan_secs: u64,
    send_begin: bool,
    csv: Option<String>,
}

fn parse_args() -> Args {
    let mut a = Args { address: None, scan_secs: 6, send_begin: true, csv: None };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--address" => a.address = it.next(),
            "--scan-secs" => a.scan_secs = it.next().and_then(|v| v.parse().ok()).unwrap_or(6),
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

fn save_csv(path: &str, log: &[(EncoderV2Rep, u64)]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "rep,fase,mpv_mps,rom_cm,pico_mps,media_mps,acel_max,acel_med,t_unix")?;
    for (r, t) in log {
        let fase = if r.phase == Phase::Concentric { "C" } else { "E" };
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{}",
            r.rep, fase, r.mpv, r.rom, r.peak_velocity, r.avg_velocity, r.max_accel, r.avg_accel, t
        )?;
    }
    Ok(())
}

/// Escanea y devuelve (Device, etiqueta, es_v2) de los candidatos.
async fn scan(adapter: &Adapter, secs: u64) -> Result<Vec<(Device, String, bool)>, Box<dyn Error>> {
    let service: Uuid = SERVICE_UUID.parse()?;
    let mut disc = adapter.discover_devices().await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    let mut seen: HashSet<Address> = HashSet::new();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => break,
            Some(ev) = disc.next() => {
                if let bluer::AdapterEvent::DeviceAdded(addr) = ev { seen.insert(addr); }
            }
            else => break,
        }
    }
    let mut out = Vec::new();
    for addr in seen {
        let dev = match adapter.device(addr) { Ok(d) => d, Err(_) => continue };
        let uuids = dev.uuids().await.ok().flatten().unwrap_or_default();
        let is_v2 = uuids.contains(&service);
        let name = dev.name().await.ok().flatten();
        // No mostramos el nombre comercial.
        let label = if is_v2 {
            format!("encoderv2  [{addr}]")
        } else if name.is_some() {
            format!("encoder?   [{addr}]")
        } else {
            continue;
        };
        out.push((dev, label, is_v2));
    }
    out.sort_by_key(|(_, _, v2)| !*v2);
    Ok(out)
}

fn choose(cands: &[(Device, String, bool)]) -> Option<usize> {
    println!("\nEncoders disponibles:");
    for (i, (_, label, _)) in cands.iter().enumerate() {
        println!("  [{}] {}", i + 1, label);
    }
    print!("Elige un encoder (número, Enter=1): ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    let line = line.trim();
    if line.is_empty() {
        return Some(0);
    }
    line.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= cands.len()).map(|n| n - 1)
}

async fn char_by_uuid(
    dev: &Device,
    uuid_str: &str,
) -> Result<Option<bluer::gatt::remote::Characteristic>, Box<dyn Error>> {
    let want: Uuid = uuid_str.parse()?;
    for s in dev.services().await? {
        for c in s.characteristics().await? {
            if c.uuid().await? == want {
                return Ok(Some(c));
            }
        }
    }
    Ok(None)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    eprintln!(
        "OpenSPD {} · GPL-3.0-or-later, sin garantía. Software NO oficial; cliente de interoperabilidad.\n\
         Estimaciones (%1RM, velocity loss) orientativas: NO sustituyen a un profesional; \
         entrenas bajo tu propia responsabilidad.",
        env!("CARGO_PKG_VERSION")
    );
    let args = parse_args();
    let csv_path = args.csv.clone().unwrap_or_else(|| format!("sesion_ble_{}.csv", now_unix()));

    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    println!("Escaneando {}s…", args.scan_secs);
    let cands = scan(&adapter, args.scan_secs).await?;

    let device = if let Some(addr) = &args.address {
        let want: Address = addr.parse()?;
        adapter.device(want)?
    } else {
        if cands.is_empty() {
            return Err("no se encontraron encoders. Enciende el encoder y reintenta.".into());
        }
        let idx = choose(&cands).ok_or("selección inválida")?;
        let addr = cands[idx].0.address();
        adapter.device(addr)?
    };

    println!("Conectando a {}…", device.address());
    if !device.is_connected().await? {
        device.connect().await?;
    }
    // esperar a que BlueZ resuelva el GATT (de su caché, rápido)
    for _ in 0..50 {
        if device.is_services_resolved().await? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // DESBLOQUEO inmediato (si no, el encoder corta a los ~3 s)
    let key = generate_key();
    let unlock = char_by_uuid(&device, CHAR_UNLOCK).await?.ok_or("falta característica de unlock")?;
    unlock.write(&key.unlock_bytes).await?;
    println!("Llave de sesión escrita. Conectado.");

    let rep_char = char_by_uuid(&device, CHAR_REPETITION).await?.ok_or("falta característica de repetición")?;

    if args.send_begin {
        if let Some(be) = char_by_uuid(&device, CHAR_BEGIN_END).await? {
            let cmd = begin_command(20, false, 'P');
            let _ = be.write(cmd.as_bytes()).await;
            println!("Serie iniciada ({cmd}).");
        }
    }

    println!("\nGuardando en: {csv_path}");
    println!("{:>4} {:>4} | {:>6} | {:>7} | {:>6} | {:>5} | zona", "REP", "FASE", "MPV", "ROM", "PICO", "VL%");
    println!("{}", "-".repeat(72));

    let mut reasm = Reassembler::new(key.aes_key);
    let mut log: Vec<(EncoderV2Rep, u64)> = Vec::new();
    let mut concentric: Vec<Rep> = Vec::new();

    let notifs = rep_char.notify().await?;
    tokio::pin!(notifs);
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    loop {
        tokio::select! {
            _ = &mut ctrl_c => { println!("\n[fin]"); break; }
            maybe = notifs.next() => {
                let Some(value) = maybe else { println!("\n[stream cerrado]"); break; };
                for msg in reasm.push(&value) {
                    if let Some(rep) = parse_repetition(&msg) {
                        log.push((rep, now_unix()));
                        let fase = if rep.phase == Phase::Concentric { "C" } else { "E" };
                        let vl = if rep.phase == Phase::Concentric {
                            concentric.push(rep.to_rep());
                            velocity_loss(&concentric)
                        } else { 0.0 };
                        println!(
                            "{:>4} {:>4} | {:>6.2} | {:>7.2} | {:>6.2} | {:>4.1}% | {}",
                            rep.rep, fase, rep.mpv, rep.rom, rep.peak_velocity, vl, load_zone(rep.mpv)
                        );
                        let _ = save_csv(&csv_path, &log);
                    }
                }
            }
        }
    }

    if args.send_begin {
        if let Some(be) = char_by_uuid(&device, CHAR_BEGIN_END).await? {
            let _ = be.write(END_SET_COMMAND.as_bytes()).await;
        }
    }
    let _ = device.disconnect().await;
    println!("Reps registradas: {} · CSV: {}", log.len(), csv_path);
    Ok(())
}
