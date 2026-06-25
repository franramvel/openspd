// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Protocolo del encoder VBT "v2" (BLE) — documentado **observando el tráfico BLE de un dispositivo
//! propio** (captura HCI), no a partir de software de terceros.
//!
//! Lo observable en la captura:
//! - Nada más conectar, el central ESCRIBE una "llave" (texto) en la característica de desbloqueo;
//!   si no se escribe, el encoder corta la conexión a los ~3 s.
//! - El stream de cada repetición llega cifrado en bloques de 16 bytes. Descifra con **AES-128-ECB**
//!   usando como clave los **primeros 16 bytes de la llave que escribió el propio cliente** (el cliente
//!   elige la clave; el encoder cifra sus datos con ella). Se confirma porque al descifrar la
//!   captura con esa clave aparece texto ASCII legible.
//! - Estructura de la llave vista en la captura: 19 caracteres, con un dígito en los índices 2, 5 y
//!   9 y una 'x' en el índice 15. Capturando varios handshakes (la app emite una llave nueva por
//!   conexión) se observa además el rango de esos dígitos (idx2: 7-9, idx9: 0-3). El encoder solo
//!   empieza a emitir si la llave cumple ese patrón.
//! - Texto plano de cada repetición (entre '@' y '&'):
//!     @<rep>*<mpv>#<rom>$<peakVel>?<avgVel>/<maxAccel><C|E><avgAccel>&
//!   (C = fase concéntrica, E = excéntrica). El recorrido crudo llega como `@N$v1$v2$…`.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, KeyInit};
use aes::Aes128;
use rand::Rng;

use crate::protocol::Rep;

// UUIDs (observables escaneando el GATT del propio dispositivo).
pub const SERVICE_UUID: &str = "4fafc201-1fb5-459e-8fcc-c5c9c331914b";
pub const CHAR_UNLOCK: &str = "bdd9354f-57be-455a-9756-e4c81a21a526"; // escribir la llave aquí
pub const CHAR_BEGIN_END: &str = "0311be00-2c80-4ead-81bd-3410a7055abb"; // begin/end de serie
pub const CHAR_REPETITION: &str = "8bfe4af5-79ad-4858-903a-b670c659b9f2"; // notify: reps cifradas
pub const CHAR_TEETH: &str = "8bcdeaf5-79ad-4858-903a-b670c659b9f2"; // notify: recorrido crudo

pub const END_SET_COMMAND: &str = "F26/9!7";

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!#/()[]{}-_=*+";

/// Llave de sesión: la cadena de 19 chars que se escribe en el encoder, y los 16 bytes (sus
/// primeros 16 chars) que se usan como clave AES.
pub struct SessionKey {
    pub unlock_bytes: Vec<u8>, // 19 bytes, se escriben en CHAR_UNLOCK
    pub aes_key: [u8; 16],     // primeros 16 chars, clave AES
}

/// Genera una llave nueva siguiendo el patrón observado en los handshakes capturados: 19 chars con
/// un dígito en idx 2 (7-9), idx 5 (0-9) e idx 9 (0-3), y 'x' en idx 15; el resto aleatorio. El
/// encoder exige ese patrón para empezar a emitir. La clave AES son los primeros 16 chars.
pub fn generate_key() -> SessionKey {
    let mut rng = rand::thread_rng();
    let mut chars: Vec<u8> = (0..15)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())])
        .collect();
    // inserciones en orden (cada una desplaza las siguientes), patrón observado en las capturas
    chars.insert(2, b'0' + rng.gen_range(7..=9));
    chars.insert(5, b'0' + rng.gen_range(0..=9));
    chars.insert(9, b'0' + rng.gen_range(0..=3));
    chars.insert(15, b'x');
    // ahora chars.len() == 19
    let mut aes_key = [0u8; 16];
    aes_key.copy_from_slice(&chars[..16]);
    SessionKey { unlock_bytes: chars, aes_key }
}

/// Descifra AES-128-ECB/NoPadding. La entrada debe ser múltiplo de 16 bytes.
pub fn decrypt_ecb(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut out = Vec::with_capacity(data.len());
    for chunk in data.chunks(16) {
        if chunk.len() != 16 {
            break;
        }
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.decrypt_block(&mut block);
        out.extend_from_slice(&block);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Concentric,
    Eccentric,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EncoderV2Rep {
    pub rep: u32,
    pub phase: Phase,
    pub mpv: f64,           // velocidad media propulsiva (m/s)
    pub rom: f64,           // recorrido / ROM (cm)
    pub peak_velocity: f64, // m/s
    pub avg_velocity: f64,  // m/s
    pub max_accel: f64,
    pub avg_accel: f64,
}

impl EncoderV2Rep {
    /// Convierte a la `Rep` común (para reutilizar métricas/perfil). Usa mpv como velocidad media.
    pub fn to_rep(&self) -> Rep {
        Rep {
            rep: self.rep,
            mean_velocity: self.mpv,
            rom: self.rom,
            peak_velocity: self.peak_velocity,
        }
    }
}

/// Parsea el contenido de una repetición (cadena que contiene `@...&`).
/// Formato: @<rep>*<mpv>#<rom>$<peak>?<avg>/<maxAcc><C|E><avgAcc>&
pub fn parse_repetition(s: &str) -> Option<EncoderV2Rep> {
    let start = s.find('@')?;
    let end = s[start..].find('&')? + start;
    let body = &s[start + 1..end]; // entre @ y &
    let (rep_s, rest) = body.split_once('*')?;
    let (mpv_s, rest) = rest.split_once('#')?;
    let (rom_s, rest) = rest.split_once('$')?;
    let (peak_s, rest) = rest.split_once('?')?;
    let (avg_s, rest) = rest.split_once('/')?;
    // rest = <maxAcc><C|E><avgAcc>
    let pi = rest.find(['C', 'E'])?;
    let max_acc_s = &rest[..pi];
    let phase = if rest.as_bytes()[pi] == b'C' { Phase::Concentric } else { Phase::Eccentric };
    let avg_acc_s = &rest[pi + 1..];
    Some(EncoderV2Rep {
        rep: rep_s.trim().parse().ok()?,
        phase,
        mpv: mpv_s.trim().parse().ok()?,
        rom: rom_s.trim().parse().ok()?,
        peak_velocity: peak_s.trim().parse().ok()?,
        avg_velocity: avg_s.trim().parse().ok()?,
        max_accel: max_acc_s.trim().parse().ok()?,
        avg_accel: avg_acc_s.trim().parse().ok()?,
    })
}

/// Reensambla notificaciones cifradas en mensajes completos `@...&`.
pub struct Reassembler {
    key: [u8; 16],
    buf: String,
}

impl Reassembler {
    pub fn new(key: [u8; 16]) -> Self {
        Reassembler { key, buf: String::new() }
    }

    /// Procesa una notificación cifrada y devuelve los mensajes `@...&` completos formados.
    pub fn push(&mut self, ciphertext: &[u8]) -> Vec<String> {
        let plain = decrypt_ecb(&self.key, ciphertext);
        let text: String = plain.iter().map(|&b| b as char).collect();
        self.buf.push_str(&text);
        let mut out = Vec::new();
        loop {
            let Some(at) = self.buf.find('@') else {
                self.buf.clear();
                break;
            };
            if at > 0 {
                self.buf.drain(..at); // descartar basura antes de @
            }
            let Some(amp) = self.buf.find('&') else { break };
            let msg: String = self.buf[..=amp].to_string();
            out.push(msg);
            self.buf.drain(..=amp);
            while self.buf.starts_with('&') {
                self.buf.remove(0);
            }
        }
        out
    }
}

/// Comando de inicio de serie: `S<rom><C|E><metric>!7` (formato observado en la captura).
pub fn begin_command(rom: u32, eccentric: bool, metric: char) -> String {
    format!("S{}{}{}!7", rom, if eccentric { "E" } else { "C" }, metric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockEncrypt;

    // Cifrado AES-128-ECB SOLO para los tests (vectores sintéticos propios, sin datos de terceros).
    fn encrypt_ecb(key: &[u8; 16], data: &[u8]) -> Vec<u8> {
        let cipher = Aes128::new(GenericArray::from_slice(key));
        let mut out = Vec::new();
        for chunk in data.chunks(16) {
            let mut b = [0u8; 16];
            b[..chunk.len()].copy_from_slice(chunk);
            let mut block = GenericArray::clone_from_slice(&b);
            cipher.encrypt_block(&mut block);
            out.extend_from_slice(&block);
        }
        out
    }

    #[test]
    fn aes_roundtrip() {
        let key = b"0123456789ABCDEF";
        let pt = b"@1*0.50#40.00$0."; // 16 bytes
        let ct = encrypt_ecb(key, pt);
        assert_eq!(&decrypt_ecb(key, &ct)[..], &pt[..]);
    }

    #[test]
    fn reensambla_y_parsea_rep_sintetica() {
        // mensaje sintético propio, rellenado con '&' a múltiplo de 16 y cifrado con nuestra clave
        let key = b"clientKey-123456";
        let mut msg = String::from("@7*1.20#55.50$1.80?1.05/9.40C0.30&");
        while msg.len() % 16 != 0 {
            msg.push('&');
        }
        let ct = encrypt_ecb(key, msg.as_bytes());
        let mut r = Reassembler::new(*key);
        let mut msgs = Vec::new();
        for chunk in ct.chunks(16) {
            msgs.extend(r.push(chunk));
        }
        assert_eq!(msgs.len(), 1);
        let rep = parse_repetition(&msgs[0]).unwrap();
        assert_eq!(rep.rep, 7);
        assert_eq!(rep.phase, Phase::Concentric);
        assert!((rep.mpv - 1.20).abs() < 1e-9);
        assert!((rep.rom - 55.50).abs() < 1e-9);
        assert!((rep.peak_velocity - 1.80).abs() < 1e-9);
        assert!((rep.avg_velocity - 1.05).abs() < 1e-9);
        assert!((rep.max_accel - 9.40).abs() < 1e-9);
        assert!((rep.avg_accel - 0.30).abs() < 1e-9);
    }

    #[test]
    fn parse_fase_excentrica() {
        let rep = parse_repetition("@3*0.80#44.0$1.10?0.75/5.0E-0.40&").unwrap();
        assert_eq!(rep.phase, Phase::Eccentric);
        assert!((rep.avg_accel - (-0.40)).abs() < 1e-9);
    }

    #[test]
    fn llave_patron_observado() {
        for _ in 0..200 {
            let k = generate_key();
            assert_eq!(k.unlock_bytes.len(), 19);
            assert_eq!(&k.aes_key[..], &k.unlock_bytes[..16]);
            let c = &k.unlock_bytes;
            assert!(c[2].is_ascii_digit());
            assert!(c[5].is_ascii_digit());
            assert!(c[9].is_ascii_digit());
            assert_eq!(c[15], b'x');
        }
    }

    #[test]
    fn begin_cmd() {
        assert_eq!(begin_command(20, false, 'P'), "S20CP!7");
    }
}
