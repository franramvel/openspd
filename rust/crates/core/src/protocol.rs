// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Protocolo del encoder VBT (1a gen, WiFi).
//!
//! El encoder es su propio AP WiFi. El cliente se conecta por TCP a 192.168.4.1:80 y debe enviarle
//! un comando de arranque `?<E>6<X><ROM>\n` (ver [`start_command`]); el encoder NO emite ninguna
//! repeticion hasta recibirlo, y lo confirma devolviendo el mismo payload tras un '!' (eco). El
//! comando ademas selecciona el ejercicio en la pantalla del encoder. Tras el eco, el encoder emite
//! una linea ASCII por CADA REPETICION, terminada en "\r\n":
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

/// ROM mínimo (cm) por defecto: umbral de recorrido para que el encoder v1 cuente una repetición.
/// Bajo a propósito para que cualquier rep válida cuente; súbelo para filtrar reps parciales.
pub const DEFAULT_ROM_CM: u32 = 10;

/// Modo de ejercicio que el encoder v1 fija en su pantalla (cada uno es un código de 1 carácter).
///
/// El encoder v1 **no es solo lectura**: al conectar hay que enviarle un comando de arranque que
/// selecciona el ejercicio; hasta recibirlo NO emite repeticiones. Ver [`start_command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExerciseV1 {
    Bench,         // press banca   -> '1'
    Squat,         // sentadilla    -> '2'
    Deadlift,      // peso muerto   -> '3'
    MilitaryPress, // press militar -> '4'
    PullUp,        // dominadas     -> '5'
    Test,          // modo test/genérico -> '9'
}

impl ExerciseV1 {
    /// Código de 1 carácter que el encoder espera para este ejercicio.
    pub fn code(self) -> char {
        match self {
            ExerciseV1::Bench => '1',
            ExerciseV1::Squat => '2',
            ExerciseV1::Deadlift => '3',
            ExerciseV1::MilitaryPress => '4',
            ExerciseV1::PullUp => '5',
            ExerciseV1::Test => '9',
        }
    }

    /// Mapea un nombre de ejercicio (es/en, con variantes) al modo del encoder v1.
    /// Devuelve `None` si el ejercicio no tiene un modo nativo en el v1.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "banca" | "press banca" | "press" | "bench" | "bench press" => Some(Self::Bench),
            "sentadilla" | "squat" => Some(Self::Squat),
            "peso muerto" | "deadlift" | "dl" => Some(Self::Deadlift),
            "press militar" | "militar" | "military" | "military press" | "ohp" => {
                Some(Self::MilitaryPress)
            }
            "dominada" | "dominadas" | "pull-up" | "pullup" | "pull up" => Some(Self::PullUp),
            "test" => Some(Self::Test),
            _ => None,
        }
    }
}

/// Construye el *payload* del comando de arranque del encoder v1 (SIN el prefijo `?`).
///
/// Formato observado en la app oficial: `<E>6<X><ROM>` donde
///   - `E`   = `'2'` concéntrica (normal) | `'1'` si además se mide la fase excéntrica,
///   - `'6'` = modo tiempo real (constante),
///   - `X`   = código de ejercicio ([`ExerciseV1::code`]),
///   - `ROM` = umbral mínimo de recorrido (cm) para contar una repetición.
///
/// El encoder devuelve este mismo payload como eco tras un `!` al confirmarlo, y **no emite
/// repeticiones hasta recibirlo**. El framing de transporte (el `?` delante y el `\n` final) lo
/// añade la capa de I/O. Ejemplo: banca, ROM 15, concéntrica → `"26115"` (se envía `?26115\n`).
pub fn start_command(ex: ExerciseV1, rom_cm: u32, eccentric: bool) -> String {
    let phase = if eccentric { '1' } else { '2' };
    format!("{phase}6{}{}", ex.code(), rom_cm)
}

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

    #[test]
    fn comando_arranque_formato() {
        // Validado en vivo: banca ROM 15 concéntrica -> "26115" (se envía ?26115\n, eco !26115).
        assert_eq!(start_command(ExerciseV1::Bench, 15, false), "26115");
        // press militar ROM 50 concéntrica
        assert_eq!(start_command(ExerciseV1::MilitaryPress, 50, false), "26450");
        // peso muerto ROM 10 midiendo excéntrica -> E='1'
        assert_eq!(start_command(ExerciseV1::Deadlift, 10, true), "16310");
    }

    #[test]
    fn nombres_a_ejercicio_v1() {
        assert_eq!(ExerciseV1::from_name("Press banca"), Some(ExerciseV1::Bench));
        assert_eq!(ExerciseV1::from_name("bench"), Some(ExerciseV1::Bench));
        assert_eq!(ExerciseV1::from_name("peso muerto"), Some(ExerciseV1::Deadlift));
        assert_eq!(ExerciseV1::from_name("DL"), Some(ExerciseV1::Deadlift));
        assert_eq!(ExerciseV1::from_name("press militar"), Some(ExerciseV1::MilitaryPress));
        assert_eq!(ExerciseV1::from_name("remo"), None);
    }
}
