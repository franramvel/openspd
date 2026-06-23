// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Adaptadores de transporte de ESCRITORIO para el encoder VBT.
//!
//! Centraliza TODO el I/O que antes estaba duplicado en los binarios TUI/GUI/BLE:
//!   - TCP (encoder v1, WiFi)            -> [`spawn_tcp_reader`]
//!   - BLE escaneo (encoder v2)          -> [`spawn_ble_scan`]
//!   - BLE lectura concéntrica (v2)      -> [`spawn_ble_reader`]  (para dashboards: solo `Rep`)
//!   - BLE lectura completa (v2)         -> [`spawn_ble_reader_full`] (todas las fases, `EncoderV2Rep`)
//!   - Persistencia de perfil y CSV      -> [`save_profile`]/[`load_profile`]/[`save_session_csv`]…
//!
//! Cada lector corre en su propio hilo (con runtime tokio propio para BLE) y emite eventos por un
//! canal `mpsc`. La UI solo consume el canal y pinta; no toca sockets, bluer ni tokio.
//!
//! En MÓVIL este crate no se usa: Android/iOS hacen el TCP/BLE nativo y llaman a `openspd-core`
//! (p. ej. `Reassembler::push` + `parse_repetition` para v2, `parse_line` para v1).

use std::collections::HashSet;
use std::error::Error;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use bluer::{Adapter, AdapterEvent, Address, Device, Session};
use futures::StreamExt;
use uuid::Uuid;

use openspd_core::encoderv2::{
    begin_command, generate_key, parse_repetition, EncoderV2Rep, Phase, Reassembler,
    CHAR_BEGIN_END, CHAR_REPETITION, CHAR_UNLOCK, END_SET_COMMAND, SERVICE_UUID,
};
use openspd_core::profile::Lvp;
use openspd_core::protocol::{parse_line, Rep};

/// Tiempo de escaneo BLE por defecto (segundos).
pub const DEFAULT_SCAN_SECS: u64 = 6;

/// Evento de un lector que produce la `Rep` común (v1 TCP o v2 concéntrica). Es el canal que
/// consumen los dashboards (TUI/GUI), agnósticos al transporte.
pub enum RepEvent {
    Status(String),
    Rep(Rep),
    Closed,
}

/// Evento de un lector BLE v2 COMPLETO: conserva todas las fases y métricas (`EncoderV2Rep`).
/// Lo usa el cliente BLE dedicado, que guarda el detalle (aceleraciones, fase excéntrica).
pub enum BleEvent {
    Status(String),
    Rep(EncoderV2Rep),
    Closed,
}

// ─────────────────────────────── TCP (encoder v1) ───────────────────────────────

/// Conecta por TCP al encoder v1 en un hilo y emite cada repetición parseada por el canal.
pub fn spawn_tcp_reader(host: String, port: u16) -> Receiver<RepEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        use std::net::TcpStream;
        let _ = tx.send(RepEvent::Status(format!("conectando a {host}:{port}…")));
        let stream = match TcpStream::connect((host.as_str(), port)) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(RepEvent::Status(format!("ERROR de red: {e} (¿wifi al encoder?)")));
                return;
            }
        };
        let _ = tx.send(RepEvent::Status("conectado · esperando reps".into()));
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if let Some(rep) = parse_line(&l) {
                        if tx.send(RepEvent::Rep(rep)).is_err() {
                            return;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(RepEvent::Closed);
    });
    rx
}

// ─────────────────────────────── BLE (encoder v2) ───────────────────────────────

/// Busca la característica con el UUID dado recorriendo los servicios resueltos del dispositivo.
async fn ble_char(dev: &Device, uuid_str: &str) -> Option<bluer::gatt::remote::Characteristic> {
    let want: Uuid = uuid_str.parse().ok()?;
    for s in dev.services().await.ok()? {
        for c in s.characteristics().await.ok()? {
            if c.uuid().await.ok()? == want {
                return Some(c);
            }
        }
    }
    None
}

/// Conexión BLE común: enciende el adaptador, conecta, espera a que se resuelva el GATT, escribe la
/// llave de desbloqueo (si no, el encoder corta a los ~3 s) y opcionalmente envía el comando de
/// inicio de serie. Devuelve el `Device` (mantenlo vivo), la característica de repeticiones y la
/// clave AES de la sesión.
async fn ble_open(
    addr: &str,
    send_begin: bool,
) -> Result<(Device, bluer::gatt::remote::Characteristic, [u8; 16]), Box<dyn Error>> {
    let session = Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    let address: Address = addr.parse().map_err(|_| "dirección inválida")?;
    let device = adapter.device(address)?;
    if !device.is_connected().await? {
        device.connect().await?;
    }
    for _ in 0..50 {
        if device.is_services_resolved().await? {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let key = generate_key();
    let unlock = ble_char(&device, CHAR_UNLOCK).await.ok_or("falta unlock")?;
    unlock.write(&key.unlock_bytes).await?;
    let rep_char = ble_char(&device, CHAR_REPETITION).await.ok_or("falta rep")?;
    if send_begin {
        if let Some(be) = ble_char(&device, CHAR_BEGIN_END).await {
            let _ = be.write(begin_command(20, false, 'P').as_bytes()).await;
        }
    }
    Ok((device, rep_char, key.aes_key))
}

/// Escanea encoders v2 (los que anuncian el `SERVICE_UUID`) durante `timeout_secs`. Devuelve una
/// lista de `(dirección, etiqueta)` por el canal.
pub fn spawn_ble_scan(timeout_secs: u64) -> Receiver<Result<Vec<(String, String)>, String>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                return;
            }
        };
        let res = rt.block_on(async {
            let service: Uuid = SERVICE_UUID.parse().map_err(|_| "uuid".to_string())?;
            let session = Session::new().await.map_err(|e| e.to_string())?;
            let adapter = session.default_adapter().await.map_err(|e| e.to_string())?;
            adapter.set_powered(true).await.map_err(|e| e.to_string())?;
            let mut disc = adapter.discover_devices().await.map_err(|e| e.to_string())?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
            let mut seen: HashSet<Address> = HashSet::new();
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    Some(ev) = disc.next() => {
                        if let AdapterEvent::DeviceAdded(a) = ev { seen.insert(a); }
                    }
                    else => break,
                }
            }
            Ok::<_, String>(collect_v2(&adapter, seen, service).await)
        });
        let _ = tx.send(res);
    });
    rx
}

/// De las direcciones vistas, queda con las que exponen el servicio del encoder v2.
async fn collect_v2(adapter: &Adapter, seen: HashSet<Address>, service: Uuid) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for addr in seen {
        if let Ok(dev) = adapter.device(addr) {
            let uuids = dev.uuids().await.ok().flatten().unwrap_or_default();
            if uuids.contains(&service) {
                out.push((addr.to_string(), format!("encoderv2 [{addr}]")));
            }
        }
    }
    out
}

/// Lector BLE v2 para dashboards: conecta, se suscribe y emite SOLO la fase concéntrica como `Rep`
/// común (`RepEvent`), igual que el lector TCP. Pensado para TUI/GUI.
pub fn spawn_ble_reader(addr: String) -> Receiver<RepEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(RepEvent::Status(e.to_string()));
                return;
            }
        };
        rt.block_on(async move {
            let _ = tx.send(RepEvent::Status(format!("conectando (v2) a {addr}…")));
            let run = async {
                let (device, rep_char, aes) = ble_open(&addr, true).await?;
                let _ = tx.send(RepEvent::Status("conectado (v2) · esperando reps".into()));
                let notifs = rep_char.notify().await?;
                tokio::pin!(notifs);
                let mut reasm = Reassembler::new(aes);
                while let Some(val) = notifs.next().await {
                    for msg in reasm.push(&val) {
                        if let Some(rep) = parse_repetition(&msg) {
                            if rep.phase == Phase::Concentric
                                && tx.send(RepEvent::Rep(rep.to_rep())).is_err()
                            {
                                return Ok(());
                            }
                        }
                    }
                }
                drop(device); // mantener el Device vivo hasta aquí
                Ok::<(), Box<dyn Error>>(())
            };
            if let Err(e) = run.await {
                let _ = tx.send(RepEvent::Status(format!("error v2: {e}")));
            }
            let _ = tx.send(RepEvent::Closed);
        });
    });
    rx
}

/// Lector BLE v2 COMPLETO: emite todas las repeticiones (ambas fases) como `EncoderV2Rep` por
/// `BleEvent`, hasta Ctrl-C o cierre del stream. Al terminar envía el comando de fin de serie (si
/// `send_begin`) y desconecta. Pensado para el cliente BLE dedicado que guarda el detalle.
pub fn spawn_ble_reader_full(addr: String, send_begin: bool) -> Receiver<BleEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(BleEvent::Status(e.to_string()));
                return;
            }
        };
        rt.block_on(async move {
            let _ = tx.send(BleEvent::Status(format!("conectando (v2) a {addr}…")));
            let run = async {
                let (device, rep_char, aes) = ble_open(&addr, send_begin).await?;
                let _ = tx.send(BleEvent::Status("conectado (v2) · esperando reps".into()));
                let notifs = rep_char.notify().await?;
                tokio::pin!(notifs);
                let mut reasm = Reassembler::new(aes);
                let ctrl_c = tokio::signal::ctrl_c();
                tokio::pin!(ctrl_c);
                loop {
                    tokio::select! {
                        _ = &mut ctrl_c => break,
                        maybe = notifs.next() => {
                            let Some(val) = maybe else { break; };
                            for msg in reasm.push(&val) {
                                if let Some(rep) = parse_repetition(&msg) {
                                    if tx.send(BleEvent::Rep(rep)).is_err() {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                    }
                }
                if send_begin {
                    if let Some(be) = ble_char(&device, CHAR_BEGIN_END).await {
                        let _ = be.write(END_SET_COMMAND.as_bytes()).await;
                    }
                }
                let _ = device.disconnect().await;
                Ok::<(), Box<dyn Error>>(())
            };
            if let Err(e) = run.await {
                let _ = tx.send(BleEvent::Status(format!("error v2: {e}")));
            }
            let _ = tx.send(BleEvent::Closed);
        });
    });
    rx
}

// ─────────────────────────────── Persistencia (escritorio) ───────────────────────────────

/// Guarda un perfil carga-velocidad a un fichero (envoltorio fino sobre [`Lvp::to_text`]).
pub fn save_profile(path: &str, lvp: &Lvp) -> std::io::Result<()> {
    std::fs::write(path, lvp.to_text())
}

/// Carga un perfil desde un fichero y lo re-ajusta (envoltorio fino sobre [`Lvp::from_text`]).
pub fn load_profile(path: &str) -> std::io::Result<Lvp> {
    let text = std::fs::read_to_string(path)?;
    Lvp::from_text(&text).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "perfil con <2 cargas validas")
    })
}

/// Escribe el CSV de sesión (formato común v1/concéntrica) de forma íntegra.
pub fn save_session_csv(path: &str, log: &[(u32, Rep, u64)]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "set,rep,vel_media_mps,rom_cm,vel_maxima_mps,t_unix")?;
    for (set, r, t) in log {
        writeln!(f, "{},{},{},{},{},{}", set, r.rep, r.mean_velocity, r.rom, r.peak_velocity, t)?;
    }
    Ok(())
}

/// Escribe el CSV detallado del encoder v2 (ambas fases, aceleraciones).
pub fn save_ble_csv(path: &str, log: &[(EncoderV2Rep, u64)]) -> std::io::Result<()> {
    use std::io::Write;
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
