// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Perfil carga-velocidad individual (Load-Velocity Profile, LVP).
//!
//! Modelo: la velocidad media cae linealmente con la carga ->  v = a + b·carga  (b < 0).
//! Ajustando esa recta con tus datos (carga, mejor velocidad) y conociendo el umbral de
//! velocidad al que se levanta el 1RM (V1RM, propio del ejercicio), se obtiene:
//!   1RM (kg)   = (V1RM - a) / b
//!   %1RM(v)    = 100·(a - v)/(a - V1RM)     (=100 cuando v=V1RM, =0 cuando v=a)
//! Es individual: mucho mas preciso que las ecuaciones poblacionales de metrics.rs.
//!
//! La persistencia es PURA: [`Lvp::to_text`] serializa a texto y [`Lvp::from_text`] reconstruye
//! (re-ajustando). Este crate NO toca el filesystem; quien quiera leer/escribir ficheros
//! (escritorio) envuelve esto con `File` (ver `openspd-io`), y en móvil se persiste el `String`
//! con los medios de la plataforma.

/// Un punto del perfil: una carga y la mejor velocidad media lograda a esa carga.
#[derive(Debug, Clone, Copy)]
pub struct Point {
    pub load_kg: f64,
    pub best_velocity: f64,
}

#[derive(Debug, Clone)]
pub struct Lvp {
    pub exercise: String,
    pub intercept: f64, // a  (velocidad extrapolada a carga 0)
    pub slope: f64,     // b  (m/s por kg, negativo)
    pub v1rm: f64,      // umbral de velocidad al 1RM
    pub one_rm_kg: f64,
    pub r2: f64,
    pub points: Vec<Point>,
}

/// Umbral de velocidad al 1RM (velocidad media) por ejercicio. Valores de referencia.
pub fn default_v1rm(exercise: &str) -> f64 {
    match exercise.to_lowercase().as_str() {
        "banca" | "bench" | "press" => 0.17,
        "sentadilla" | "squat" => 0.30,
        "peso muerto" | "deadlift" => 0.50,
        _ => 0.30,
    }
}

/// Ajusta la recta por minimos cuadrados. Necesita >=2 cargas distintas.
pub fn fit(exercise: &str, points: Vec<Point>, v1rm: f64) -> Option<Lvp> {
    if points.len() < 2 {
        return None;
    }
    let n = points.len() as f64;
    let mx = points.iter().map(|p| p.load_kg).sum::<f64>() / n;
    let my = points.iter().map(|p| p.best_velocity).sum::<f64>() / n;
    let sxx: f64 = points.iter().map(|p| (p.load_kg - mx).powi(2)).sum();
    let sxy: f64 = points
        .iter()
        .map(|p| (p.load_kg - mx) * (p.best_velocity - my))
        .sum();
    if sxx == 0.0 {
        return None; // todas las cargas iguales
    }
    let b = sxy / sxx;
    let a = my - b * mx;
    let ss_tot: f64 = points.iter().map(|p| (p.best_velocity - my).powi(2)).sum();
    let ss_res: f64 = points
        .iter()
        .map(|p| (p.best_velocity - (a + b * p.load_kg)).powi(2))
        .sum();
    let r2 = if ss_tot > 0.0 { 1.0 - ss_res / ss_tot } else { 1.0 };
    let one_rm_kg = if b != 0.0 { (v1rm - a) / b } else { 0.0 };
    Some(Lvp {
        exercise: exercise.to_string(),
        intercept: a,
        slope: b,
        v1rm,
        one_rm_kg,
        r2,
        points,
    })
}

impl Lvp {
    /// %1RM correspondiente a una velocidad media.
    pub fn pct_1rm(&self, velocity: f64) -> f64 {
        let denom = self.intercept - self.v1rm;
        if denom == 0.0 {
            return 0.0;
        }
        (100.0 * (self.intercept - velocity) / denom).clamp(0.0, 150.0)
    }

    /// Carga estimada (kg) para una velocidad media dada.
    pub fn load_for_velocity(&self, v: f64) -> f64 {
        if self.slope != 0.0 {
            (v - self.intercept) / self.slope
        } else {
            0.0
        }
    }

    /// Velocidad media esperada a un %1RM dado.
    pub fn velocity_for_pct(&self, pct: f64) -> f64 {
        self.intercept + (self.v1rm - self.intercept) * (pct / 100.0)
    }

    /// True si el perfil es coherente (la velocidad baja al subir la carga).
    pub fn is_valid(&self) -> bool {
        self.slope < 0.0 && self.one_rm_kg > 0.0
    }
}

impl Lvp {
    /// Serializa el perfil (ejercicio, v1rm y puntos) a texto. Al reconstruir se re-ajusta.
    /// Puro: no toca el filesystem. El escritorio lo escribe a un `.lvp`; móvil lo guarda donde
    /// quiera.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("exercise={}\n", self.exercise));
        s.push_str(&format!("v1rm={}\n", self.v1rm));
        s.push_str("# load_kg,best_velocity\n");
        for p in &self.points {
            s.push_str(&format!("{},{}\n", p.load_kg, p.best_velocity));
        }
        s
    }

    /// Reconstruye un perfil desde el texto de [`Lvp::to_text`], re-ajustando la recta.
    /// Devuelve `None` si no hay >=2 cargas válidas. Puro: no toca el filesystem.
    pub fn from_text(text: &str) -> Option<Lvp> {
        let mut exercise = String::from("custom");
        let mut v1rm = 0.30;
        let mut points = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(v) = line.strip_prefix("exercise=") {
                exercise = v.to_string();
            } else if let Some(v) = line.strip_prefix("v1rm=") {
                v1rm = v.trim().parse().unwrap_or(0.30);
            } else if let Some((l, vel)) = line.split_once(',') {
                if let (Ok(l), Ok(vel)) = (l.trim().parse(), vel.trim().parse()) {
                    points.push(Point { load_kg: l, best_velocity: vel });
                }
            }
        }
        fit(&exercise, points, v1rm)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ajuste_y_1rm() {
        // v = 1.5 - 0.01*carga ; con V1RM=0.3 -> 1RM = (0.3-1.5)/-0.01 = 120 kg
        let pts = vec![
            Point { load_kg: 40.0, best_velocity: 1.10 },
            Point { load_kg: 60.0, best_velocity: 0.90 },
            Point { load_kg: 80.0, best_velocity: 0.70 },
        ];
        let lvp = fit("sentadilla", pts, 0.30).unwrap();
        assert!((lvp.intercept - 1.5).abs() < 1e-6);
        assert!((lvp.slope - (-0.01)).abs() < 1e-6);
        assert!((lvp.one_rm_kg - 120.0).abs() < 1e-6);
        assert!(lvp.r2 > 0.999);
        assert!(lvp.is_valid());
        // a 60 kg -> v=0.9 -> %1RM = 60/120 = 50%
        assert!((lvp.pct_1rm(0.90) - 50.0).abs() < 1e-6);
    }

    #[test]
    fn texto_round_trip() {
        let pts = vec![
            Point { load_kg: 40.0, best_velocity: 1.10 },
            Point { load_kg: 60.0, best_velocity: 0.90 },
            Point { load_kg: 80.0, best_velocity: 0.70 },
        ];
        let lvp = fit("sentadilla", pts, 0.30).unwrap();
        let back = Lvp::from_text(&lvp.to_text()).unwrap();
        assert_eq!(back.exercise, "sentadilla");
        assert!((back.v1rm - 0.30).abs() < 1e-9);
        assert_eq!(back.points.len(), 3);
        // re-ajustar desde el texto reproduce el mismo 1RM
        assert!((back.one_rm_kg - lvp.one_rm_kg).abs() < 1e-6);
    }

    #[test]
    fn from_text_rechaza_pocos_puntos() {
        assert!(Lvp::from_text("exercise=banca\nv1rm=0.17\n40,0.5\n").is_none());
    }
}
