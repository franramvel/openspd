// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Bindings uniffi de OpenSPD: expone el dominio de `openspd-core` a Kotlin y Swift.
//!
//! Filosofía: el core se mantiene 100% puro (sin dependencia de uniffi). Aquí definimos tipos
//! "espejo" (`Record`/`Enum`/`Object`) y funciones envoltorio que convierten a/desde los tipos del
//! core. Así el mismo núcleo sirve a escritorio, móvil y (en el futuro) wasm.
//!
//! Reparto en móvil: Android/iOS hacen el TCP/BLE NATIVO y usan estas funciones para:
//!   - v1 (WiFi):  `parse_line` sobre cada línea recibida.
//!   - v2 (BLE):   `generate_key` (escribir `unlock_bytes` en la característica de desbloqueo) y un
//!                 `Reassembler` al que se le pasan los bytes cifrados de cada notificación.
//!   - métricas y perfil: `velocity_loss`, `summarize`, `est_1rm_*`, `lvp_fit`, `Lvp::*`.

use std::sync::{Arc, Mutex};

use openspd_core::{encoderv2, metrics, profile, protocol};

uniffi::setup_scaffolding!();

// ─────────────────────────────── Tipos de dominio (Records/Enums) ───────────────────────────────

/// Una repetición decodificada (modelo común v1/v2).
#[derive(uniffi::Record, Clone, Copy)]
pub struct Rep {
    pub rep: u32,
    pub mean_velocity: f64,
    pub rom: f64,
    pub peak_velocity: f64,
}

impl From<protocol::Rep> for Rep {
    fn from(r: protocol::Rep) -> Self {
        Rep { rep: r.rep, mean_velocity: r.mean_velocity, rom: r.rom, peak_velocity: r.peak_velocity }
    }
}
impl From<Rep> for protocol::Rep {
    fn from(r: Rep) -> Self {
        protocol::Rep { rep: r.rep, mean_velocity: r.mean_velocity, rom: r.rom, peak_velocity: r.peak_velocity }
    }
}

/// Resumen de una serie.
#[derive(uniffi::Record)]
pub struct SetSummary {
    pub n_reps: u32,
    pub best_mean_velocity: f64,
    pub last_mean_velocity: f64,
    pub avg_mean_velocity: f64,
    pub peak_velocity: f64,
    pub avg_rom: f64,
    pub velocity_loss_pct: f64,
}

impl From<metrics::SetSummary> for SetSummary {
    fn from(s: metrics::SetSummary) -> Self {
        SetSummary {
            n_reps: s.n_reps as u32,
            best_mean_velocity: s.best_mean_velocity,
            last_mean_velocity: s.last_mean_velocity,
            avg_mean_velocity: s.avg_mean_velocity,
            peak_velocity: s.peak_velocity,
            avg_rom: s.avg_rom,
            velocity_loss_pct: s.velocity_loss_pct,
        }
    }
}

/// Fase de una repetición del encoder v2.
#[derive(uniffi::Enum, Clone, Copy, PartialEq)]
pub enum Phase {
    Concentric,
    Eccentric,
}

impl From<encoderv2::Phase> for Phase {
    fn from(p: encoderv2::Phase) -> Self {
        match p {
            encoderv2::Phase::Concentric => Phase::Concentric,
            encoderv2::Phase::Eccentric => Phase::Eccentric,
        }
    }
}

/// Repetición completa del encoder v2 (ambas fases, aceleraciones).
#[derive(uniffi::Record, Clone, Copy)]
pub struct EncoderV2Rep {
    pub rep: u32,
    pub phase: Phase,
    pub mpv: f64,
    pub rom: f64,
    pub peak_velocity: f64,
    pub avg_velocity: f64,
    pub max_accel: f64,
    pub avg_accel: f64,
}

impl From<encoderv2::EncoderV2Rep> for EncoderV2Rep {
    fn from(r: encoderv2::EncoderV2Rep) -> Self {
        EncoderV2Rep {
            rep: r.rep,
            phase: r.phase.into(),
            mpv: r.mpv,
            rom: r.rom,
            peak_velocity: r.peak_velocity,
            avg_velocity: r.avg_velocity,
            max_accel: r.max_accel,
            avg_accel: r.avg_accel,
        }
    }
}

/// Un punto del perfil carga-velocidad.
#[derive(uniffi::Record, Clone, Copy)]
pub struct Point {
    pub load_kg: f64,
    pub best_velocity: f64,
}

impl From<profile::Point> for Point {
    fn from(p: profile::Point) -> Self {
        Point { load_kg: p.load_kg, best_velocity: p.best_velocity }
    }
}
impl From<Point> for profile::Point {
    fn from(p: Point) -> Self {
        profile::Point { load_kg: p.load_kg, best_velocity: p.best_velocity }
    }
}

/// Llave de sesión del encoder v2: escribe `unlock_bytes` en la característica de desbloqueo y usa
/// `aes_key` para crear el [`Reassembler`].
#[derive(uniffi::Record)]
pub struct SessionKey {
    pub unlock_bytes: Vec<u8>,
    pub aes_key: Vec<u8>,
}

// ─────────────────────────────── Funciones de protocolo / métricas ───────────────────────────────

/// Parsea una línea cruda del encoder v1 (WiFi). `None` si no encaja con el formato.
#[uniffi::export]
pub fn parse_line(line: String) -> Option<Rep> {
    protocol::parse_line(&line).map(Into::into)
}

/// % de pérdida de velocidad respecto a la mejor rep de la serie.
#[uniffi::export]
pub fn velocity_loss(reps: Vec<Rep>) -> f64 {
    let v: Vec<protocol::Rep> = reps.into_iter().map(Into::into).collect();
    metrics::velocity_loss(&v)
}

/// Resumen de una serie. `None` si no hay reps.
#[uniffi::export]
pub fn summarize(reps: Vec<Rep>) -> Option<SetSummary> {
    let v: Vec<protocol::Rep> = reps.into_iter().map(Into::into).collect();
    metrics::summarize(&v).map(Into::into)
}

/// %1RM estimado por ecuación poblacional validada. `None` si el ejercicio no tiene ecuación propia.
#[uniffi::export]
pub fn est_1rm_pct(exercise: String, mean_velocity: f64) -> Option<f64> {
    metrics::est_1rm_pct(&exercise, mean_velocity)
}

/// %1RM estimado: ecuación validada si existe, si no estimación genérica sugerida. Nunca `None`.
#[uniffi::export]
pub fn est_1rm_pct_any(exercise: String, mean_velocity: f64) -> f64 {
    metrics::est_1rm_pct_src(&exercise, mean_velocity).0
}

/// 1RM (kg) a partir de la carga usada y el %1RM correspondiente. `None` si %1RM <= 0.
#[uniffi::export]
pub fn est_1rm_kg(load_kg: f64, pct_1rm: f64) -> Option<f64> {
    metrics::est_1rm_kg(load_kg, pct_1rm)
}

/// Zona de carga orientativa según la velocidad media.
#[uniffi::export]
pub fn load_zone(mean_velocity: f64) -> String {
    metrics::load_zone(mean_velocity).to_string()
}

// ─────────────────────────────── Encoder v2 (BLE) ───────────────────────────────

/// Genera una llave de sesión nueva para el encoder v2.
#[uniffi::export]
pub fn generate_key() -> SessionKey {
    let k = encoderv2::generate_key();
    SessionKey { unlock_bytes: k.unlock_bytes, aes_key: k.aes_key.to_vec() }
}

/// Parsea el texto plano de una repetición v2 (`@...&`).
#[uniffi::export]
pub fn parse_repetition(s: String) -> Option<EncoderV2Rep> {
    encoderv2::parse_repetition(&s).map(Into::into)
}

/// Convierte una rep v2 al modelo común `Rep` (usa MPV como velocidad media).
#[uniffi::export]
pub fn encoder_v2_to_rep(rep: EncoderV2Rep) -> Rep {
    Rep {
        rep: rep.rep,
        mean_velocity: rep.mpv,
        rom: rep.rom,
        peak_velocity: rep.peak_velocity,
    }
}

/// Comando de inicio de serie del encoder v2.
#[uniffi::export]
pub fn begin_command(rom: u32, eccentric: bool, metric: String) -> String {
    let m = metric.chars().next().unwrap_or('P');
    encoderv2::begin_command(rom, eccentric, m)
}

/// Reensamblador con estado: la app móvil le pasa los bytes cifrados de cada notificación BLE y
/// recibe las repeticiones completas ya descifradas y parseadas.
#[derive(uniffi::Object)]
pub struct Reassembler {
    inner: Mutex<encoderv2::Reassembler>,
}

#[uniffi::export]
impl Reassembler {
    /// Crea un reensamblador con la clave AES de la sesión (16 bytes, de [`SessionKey::aes_key`]).
    #[uniffi::constructor]
    pub fn new(aes_key: Vec<u8>) -> Arc<Reassembler> {
        let mut key = [0u8; 16];
        let n = aes_key.len().min(16);
        key[..n].copy_from_slice(&aes_key[..n]);
        Arc::new(Reassembler { inner: Mutex::new(encoderv2::Reassembler::new(key)) })
    }

    /// Procesa una notificación cifrada y devuelve las repeticiones completas formadas.
    pub fn push(&self, ciphertext: Vec<u8>) -> Vec<EncoderV2Rep> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .push(&ciphertext)
            .iter()
            .filter_map(|msg| encoderv2::parse_repetition(msg))
            .map(Into::into)
            .collect()
    }
}

// ─────────────────────────────── Perfil carga-velocidad ───────────────────────────────

/// Umbral de velocidad al 1RM por defecto para un ejercicio.
#[uniffi::export]
pub fn default_v1rm(exercise: String) -> f64 {
    profile::default_v1rm(&exercise)
}

/// True si el ejercicio es de peso corporal (dominada, fondo...): la carga a registrar es la
/// carga TOTAL = peso corporal + lastre.
#[uniffi::export]
pub fn is_bodyweight(exercise: String) -> bool {
    profile::is_bodyweight(&exercise)
}

/// Carga total movida en un ejercicio de peso corporal = peso corporal + lastre (`added_kg`
/// puede ser negativo si hay asistencia). Es la carga que debe ir al perfil y al %1RM.
#[uniffi::export]
pub fn total_bodyweight_load(bodyweight_kg: f64, added_kg: f64) -> f64 {
    profile::total_bodyweight_load(bodyweight_kg, added_kg)
}

/// Ajusta un perfil carga-velocidad. `None` si hay <2 cargas distintas.
#[uniffi::export]
pub fn lvp_fit(exercise: String, points: Vec<Point>, v1rm: f64) -> Option<Arc<Lvp>> {
    let pts: Vec<profile::Point> = points.into_iter().map(Into::into).collect();
    profile::fit(&exercise, pts, v1rm).map(|inner| Arc::new(Lvp { inner }))
}

/// Reconstruye un perfil desde su texto serializado (`Lvp::to_text`). `None` si es inválido.
#[uniffi::export]
pub fn lvp_from_text(text: String) -> Option<Arc<Lvp>> {
    profile::Lvp::from_text(&text).map(|inner| Arc::new(Lvp { inner }))
}

/// Perfil carga-velocidad individual (objeto con estado inmutable).
#[derive(uniffi::Object)]
pub struct Lvp {
    inner: profile::Lvp,
}

#[uniffi::export]
impl Lvp {
    pub fn exercise(&self) -> String {
        self.inner.exercise.clone()
    }
    pub fn intercept(&self) -> f64 {
        self.inner.intercept
    }
    pub fn slope(&self) -> f64 {
        self.inner.slope
    }
    pub fn v1rm(&self) -> f64 {
        self.inner.v1rm
    }
    pub fn one_rm_kg(&self) -> f64 {
        self.inner.one_rm_kg
    }
    pub fn r2(&self) -> f64 {
        self.inner.r2
    }
    pub fn points(&self) -> Vec<Point> {
        self.inner.points.iter().copied().map(Into::into).collect()
    }
    pub fn is_valid(&self) -> bool {
        self.inner.is_valid()
    }
    /// %1RM correspondiente a una velocidad media.
    pub fn pct_1rm(&self, velocity: f64) -> f64 {
        self.inner.pct_1rm(velocity)
    }
    /// Carga estimada (kg) para una velocidad media dada.
    pub fn load_for_velocity(&self, v: f64) -> f64 {
        self.inner.load_for_velocity(v)
    }
    /// Velocidad media esperada a un %1RM dado.
    pub fn velocity_for_pct(&self, pct: f64) -> f64 {
        self.inner.velocity_for_pct(pct)
    }
    /// Serializa el perfil a texto (para persistirlo con los medios de la plataforma).
    pub fn to_text(&self) -> String {
        self.inner.to_text()
    }
}
