// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Metricas VBT (Velocity Based Training).
//!
//! La metrica de autorregulacion es el VELOCITY LOSS: cuanto cae la velocidad media
//! respecto a la mejor rep de la serie. Ej: "parar la serie al perder 20%".

use crate::protocol::Rep;

#[derive(Debug, Clone, Copy)]
pub struct SetSummary {
    pub n_reps: usize,
    pub best_mean_velocity: f64,
    pub last_mean_velocity: f64,
    pub avg_mean_velocity: f64,
    pub peak_velocity: f64,
    pub avg_rom: f64,
    pub velocity_loss_pct: f64,
}

impl std::fmt::Display for SetSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "── Resumen de la serie ──────────────────────")?;
        writeln!(f, "  Reps:                 {}", self.n_reps)?;
        writeln!(f, "  Vel. media (1a/mejor):{:6.2} m/s", self.best_mean_velocity)?;
        writeln!(f, "  Vel. media (ultima):  {:6.2} m/s", self.last_mean_velocity)?;
        writeln!(f, "  Vel. media (prom):    {:6.2} m/s", self.avg_mean_velocity)?;
        writeln!(f, "  Vel. pico (max):      {:6.2} m/s", self.peak_velocity)?;
        writeln!(f, "  ROM medio:            {:7.2} cm", self.avg_rom)?;
        writeln!(f, "  VELOCITY LOSS:        {:6.1} %", self.velocity_loss_pct)?;
        write!(f, "─────────────────────────────────────────────")
    }
}

/// % de perdida de velocidad respecto a la mejor rep de la serie.
pub fn velocity_loss(reps: &[Rep]) -> f64 {
    if reps.is_empty() {
        return 0.0;
    }
    let best = reps.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
    if best <= 0.0 {
        return 0.0;
    }
    (best - reps.last().unwrap().mean_velocity) / best * 100.0
}

pub fn summarize(reps: &[Rep]) -> Option<SetSummary> {
    if reps.is_empty() {
        return None;
    }
    let n = reps.len();
    let best = reps.iter().map(|r| r.mean_velocity).fold(f64::MIN, f64::max);
    let peak = reps.iter().map(|r| r.peak_velocity).fold(f64::MIN, f64::max);
    let avg_v = reps.iter().map(|r| r.mean_velocity).sum::<f64>() / n as f64;
    let avg_rom = reps.iter().map(|r| r.rom).sum::<f64>() / n as f64;
    Some(SetSummary {
        n_reps: n,
        best_mean_velocity: best,
        last_mean_velocity: reps.last().unwrap().mean_velocity,
        avg_mean_velocity: avg_v,
        peak_velocity: peak,
        avg_rom,
        velocity_loss_pct: velocity_loss(reps),
    })
}

/// Estima el %1RM a partir de la velocidad media usando ecuaciones carga-velocidad
/// publicadas (poblacionales). APROXIMADO: para precision construye tu propio perfil
/// carga-velocidad (registra velocidades a cargas conocidas y ajusta una recta).
///
/// - banca:      González-Badillo & Sánchez-Medina (2010)
/// - sentadilla: Sánchez-Medina et al. (2017)
///
/// Devuelve None si el ejercicio no tiene ecuacion conocida.
pub fn est_1rm_pct(exercise: &str, mean_velocity: f64) -> Option<f64> {
    let v = mean_velocity;
    let pct = match exercise.to_lowercase().as_str() {
        "banca" | "bench" | "press" => 8.4326 * v * v - 73.501 * v + 112.33,
        "sentadilla" | "squat" => -5.961 * v * v - 50.71 * v + 117.0,
        _ => return None,
    };
    Some(pct.clamp(0.0, 100.0))
}

/// Estima el 1RM en kg dado la carga usada y el %1RM al que corresponde su velocidad.
pub fn est_1rm_kg(load_kg: f64, pct_1rm: f64) -> Option<f64> {
    if pct_1rm <= 0.0 {
        return None;
    }
    Some(load_kg / (pct_1rm / 100.0))
}

/// Zona de carga orientativa (squat) segun velocidad media. Ajustar con tu perfil.
pub fn load_zone(mean_velocity: f64) -> &'static str {
    if mean_velocity >= 1.0 {
        "Fuerza-velocidad / explosivo (carga ligera)"
    } else if mean_velocity >= 0.75 {
        "Fuerza-potencia (carga media)"
    } else if mean_velocity >= 0.5 {
        "Fuerza acelerada (carga media-alta)"
    } else {
        "Fuerza maxima (carga alta)"
    }
}
