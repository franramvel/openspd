// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Protocolo del encoder VBT (1a gen, WiFi).
//!
//! El encoder es su propio AP WiFi. Te conectas por TCP a 192.168.4.1:80 y, sin pedir
//! nada, emite una linea ASCII por CADA REPETICION, terminada en "\r\n":
//!
//! ```text
//! @<rep>*<vel_media>#<rom>$<vel_maxima>&     ej: @4*1.55#55.77$2.06&
//! ```
//!
//! Confirmado contra la pantalla del encoder:
//!   rep        = numero de repeticion
//!   vel_media  = velocidad media   (m/s)
//!   rom        = recorrido / ROM   (cm)
//!   vel_maxima = velocidad pico    (m/s)  (siempre > media)

pub const ENCODER_HOST: &str = "192.168.4.1";
pub const ENCODER_PORT: u16 = 80;

/// Una repeticion decodificada.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rep {
    pub rep: u32,
    pub mean_velocity: f64, // m/s
    pub rom: f64,           // cm
    pub peak_velocity: f64, // m/s
}

/// Parsea una linea cruda del encoder. Devuelve None si no encaja con el formato.
///
/// Parser manual (sin regex): quita '@' inicial y '&' final, y separa por '*' '#' '$'.
pub fn parse_line(line: &str) -> Option<Rep> {
    let s = line.trim();
    let s = s.strip_prefix('@')?;
    let s = s.strip_suffix('&')?;
    // @rep * media # rom $ pico
    let (rep_s, rest) = s.split_once('*')?;
    let (mean_s, rest) = rest.split_once('#')?;
    let (rom_s, peak_s) = rest.split_once('$')?;
    Some(Rep {
        rep: rep_s.trim().parse().ok()?,
        mean_velocity: mean_s.trim().parse().ok()?,
        rom: rom_s.trim().parse().ok()?,
        peak_velocity: peak_s.trim().parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsea_muestras_reales() {
        assert_eq!(
            parse_line("@4*1.55#55.77$2.06&"),
            Some(Rep { rep: 4, mean_velocity: 1.55, rom: 55.77, peak_velocity: 2.06 })
        );
        assert_eq!(
            parse_line("@3*0.26#45.95$0.38&\r\n"),
            Some(Rep { rep: 3, mean_velocity: 0.26, rom: 45.95, peak_velocity: 0.38 })
        );
    }

    #[test]
    fn rechaza_basura() {
        assert_eq!(parse_line("basura"), None);
        assert_eq!(parse_line("@4*x#y$z&"), None);
        assert_eq!(parse_line(""), None);
    }
}
