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

/// Origen de una estimación de %1RM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EqSource {
    /// Ecuación carga-velocidad poblacional, publicada y validada PARA ESE ejercicio.
    Validada,
    /// Estimación GENÉRICA sugerida (el ejercicio no tiene ecuación propia validada): recta
    /// anclada en el umbral de velocidad al 1RM del ejercicio con una pendiente media. Es
    /// orientativa; construye tu perfil individual para precisión.
    Generica,
}

/// Pendiente media (%1RM por m/s) de la recta carga-velocidad: promedio representativo de la
/// literatura (p.ej. peso muerto ≈ −80, banca más pronunciada). Solo se usa en la estimación
/// genérica de los ejercicios sin ecuación propia.
const GENERIC_SLOPE: f64 = -90.0;

/// Estima el %1RM a partir de la velocidad media usando ecuaciones carga-velocidad
/// publicadas (poblacionales). APROXIMADO: para precision construye tu propio perfil
/// carga-velocidad (registra velocidades a cargas conocidas y ajusta una recta).
///
/// Solo coeficientes (datos/hechos, no protegidos por copyright), con su fuente:
/// - banca:      polinómica, González-Badillo & Sánchez-Medina (2010)
/// - sentadilla: polinómica, Sánchez-Medina et al. (2017)
/// - peso muerto: lineal, análisis carga-velocidad del peso muerto convencional (2020)
///
/// Devuelve None si el ejercicio no tiene ecuacion propia validada (usa [`est_1rm_pct_src`]
/// para obtener en ese caso una estimación genérica sugerida).
pub fn est_1rm_pct(exercise: &str, mean_velocity: f64) -> Option<f64> {
    let v = mean_velocity;
    let pct = match exercise.to_lowercase().as_str() {
        "banca" | "bench" | "press" => 8.4326 * v * v - 73.501 * v + 112.33,
        "sentadilla" | "squat" => -5.961 * v * v - 50.71 * v + 117.0,
        "peso muerto" | "deadlift" => -80.188 * v + 124.929,
        _ => return None,
    };
    Some(pct.clamp(0.0, 100.0))
}

/// %1RM genérico SUGERIDO para ejercicios sin ecuación propia: recta que pasa por (v1rm, 100%)
/// con [`GENERIC_SLOPE`]. Orientativo; el perfil individual es mucho más preciso.
pub fn est_1rm_pct_generic(v1rm: f64, mean_velocity: f64) -> f64 {
    (100.0 + GENERIC_SLOPE * (mean_velocity - v1rm)).clamp(0.0, 100.0)
}

/// %1RM y su origen: usa la ecuación validada del ejercicio si existe; si no, la genérica
/// sugerida anclada en su umbral de velocidad al 1RM ([`crate::profile::default_v1rm`]). Nunca None.
pub fn est_1rm_pct_src(exercise: &str, mean_velocity: f64) -> (f64, EqSource) {
    match est_1rm_pct(exercise, mean_velocity) {
        Some(pct) => (pct, EqSource::Validada),
        None => {
            let v1rm = crate::profile::default_v1rm(exercise);
            (est_1rm_pct_generic(v1rm, mean_velocity), EqSource::Generica)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peso_muerto_da_100pct_en_su_umbral() {
        // La recta del peso muerto cruza ~100% en su MVT (~0.31 m/s).
        let pct = est_1rm_pct("peso muerto", 0.31).unwrap();
        assert!((pct - 100.0).abs() < 2.0, "pct={pct}");
        // A velocidad alta el %1RM cae claramente.
        assert!(est_1rm_pct("deadlift", 0.8).unwrap() < 70.0);
    }

    #[test]
    fn ejercicio_sin_ecuacion_usa_generica() {
        // Dominada no tiene ecuación validada -> None en est_1rm_pct...
        assert!(est_1rm_pct("dominada", 0.5).is_none());
        // ...pero est_1rm_pct_src devuelve una estimación genérica anclada en su v1rm (0.23).
        let (pct, src) = est_1rm_pct_src("dominada", 0.23);
        assert_eq!(src, EqSource::Generica);
        assert!((pct - 100.0).abs() < 1.0, "pct={pct}");
        // Y un ejercicio con ecuación propia se marca como validada.
        assert_eq!(est_1rm_pct_src("banca", 0.5).1, EqSource::Validada);
    }
}
