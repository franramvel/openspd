// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Multiusuario (multiperfil) de ESCRITORIO: registro de usuarios y enrutado de ficheros.
//!
//! Cada persona que entrena tiene su propio directorio bajo `./users/<slug>/`; ahí caen sus CSV de
//! sesión y sus perfiles `.lvp`, sin pisar los de los demás. Esto es SOLO una capa de rutas: la
//! lógica de dominio (`openspd-core`) sigue siendo pura y no sabe nada de usuarios.
//!
//! El registro vive en `./users/index.txt`, una línea por usuario con `slug<TAB>nombre visible`,
//! para que el nombre admita espacios/acentos mientras el directorio queda seguro en el sistema de
//! ficheros. Las 4 interfaces (CLI, TUI, GUI, BLE) comparten estas funciones para no duplicar.

use std::path::PathBuf;

/// Usuario registrado: `slug` (nombre de directorio seguro) y `display` (nombre visible original).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub slug: String,
    pub display: String,
}

/// Raíz de almacenamiento multiusuario, relativa al directorio de trabajo actual.
pub fn storage_root() -> PathBuf {
    PathBuf::from("users")
}

/// Fichero de registro de usuarios.
fn index_path() -> PathBuf {
    storage_root().join("index.txt")
}

/// Plega un carácter acentuado/no-ASCII común (español) a su equivalente ASCII; el resto se deja.
fn fold_char(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' => 'a',
        'é' | 'è' | 'ë' | 'ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' => 'u',
        'ñ' => 'n',
        'ç' => 'c',
        _ => c,
    }
}

/// Convierte un nombre visible en un nombre de directorio seguro: minúsculas, sin acentos
/// (á→a, ñ→n…), espacios y puntuación → `_` (sin repetir ni dejar bordes), y `"user"` si queda
/// vacío. Se usa tanto para el usuario como para el ejercicio del fichero de perfil.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = false;
    for c in name.trim().chars().flat_map(|c| c.to_lowercase()) {
        let f = fold_char(c);
        if f.is_ascii_alphanumeric() {
            out.push(f);
            prev_sep = false;
        } else if !prev_sep && !out.is_empty() {
            out.push('_');
            prev_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "user".to_string()
    } else {
        out
    }
}

/// Lee el registro crudo (vacío si el fichero no existe).
fn read_index() -> Vec<User> {
    let text = match std::fs::read_to_string(index_path()) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut users = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (slug, display) = match line.split_once('\t') {
            Some((s, d)) => (s.to_string(), d.to_string()),
            None => (line.to_string(), line.to_string()),
        };
        users.push(User { slug, display });
    }
    users
}

/// Lista los usuarios registrados, ordenados por nombre visible. Si no hay registro todavía,
/// devuelve un único usuario sintético `default` (compatibilidad con el flujo de un solo usuario).
pub fn list_users() -> std::io::Result<Vec<User>> {
    let mut users = read_index();
    if users.is_empty() {
        return Ok(vec![User { slug: "default".into(), display: "default".into() }]);
    }
    users.sort_by_key(|u| u.display.to_lowercase());
    Ok(users)
}

/// `./users/<slug>` (lo crea si no existe).
pub fn user_dir(slug: &str) -> std::io::Result<PathBuf> {
    let dir = storage_root().join(slug);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Asegura que un usuario existe: calcula su slug, crea su directorio y lo añade al registro.
/// Idempotente: si ya hay un usuario con el mismo slug, lo devuelve sin duplicar.
pub fn add_user(display: &str) -> std::io::Result<User> {
    let slug = slugify(display);
    let existing = read_index();
    if let Some(u) = existing.iter().find(|u| u.slug == slug) {
        // ya registrado: garantizar el directorio y devolverlo
        user_dir(&u.slug)?;
        return Ok(u.clone());
    }
    let display = display.trim();
    let display = if display.is_empty() { slug.clone() } else { display.to_string() };
    user_dir(&slug)?;
    // crear/abrir el índice y añadir la línea
    use std::io::Write;
    std::fs::create_dir_all(storage_root())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(index_path())?;
    writeln!(f, "{slug}\t{display}")?;
    Ok(User { slug, display })
}

/// Quita un usuario del registro. Si `purge`, además borra su directorio y todo su contenido.
pub fn remove_user(slug: &str, purge: bool) -> std::io::Result<()> {
    let remaining: Vec<User> = read_index().into_iter().filter(|u| u.slug != slug).collect();
    use std::io::Write;
    std::fs::create_dir_all(storage_root())?;
    let mut f = std::fs::File::create(index_path())?;
    for u in &remaining {
        writeln!(f, "{}\t{}", u.slug, u.display)?;
    }
    if purge {
        let dir = storage_root().join(slug);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
    }
    Ok(())
}

/// Ruta del perfil carga-velocidad de un usuario para un ejercicio: `./users/<slug>/<ejercicio>.lvp`.
pub fn profile_path_for(slug: &str, exercise: &str) -> std::io::Result<String> {
    let dir = user_dir(slug)?;
    let file = format!("{}.lvp", slugify(exercise));
    Ok(dir.join(file).to_string_lossy().into_owned())
}

/// Ruta del CSV de sesión (formato común v1/concéntrica) de un usuario.
pub fn session_csv_path_for(slug: &str, unix: u64) -> std::io::Result<String> {
    let dir = user_dir(slug)?;
    Ok(dir.join(format!("sesion_{unix}.csv")).to_string_lossy().into_owned())
}

/// Ruta del CSV detallado BLE (encoder v2) de un usuario.
pub fn ble_csv_path_for(slug: &str, unix: u64) -> std::io::Result<String> {
    let dir = user_dir(slug)?;
    Ok(dir.join(format!("sesion_ble_{unix}.csv")).to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_folds_accents_and_separators() {
        assert_eq!(slugify("Péña Júnior"), "pena_junior");
        assert_eq!(slugify("  Ana  "), "ana");
        assert_eq!(slugify("José-María  López"), "jose_maria_lopez");
        assert_eq!(slugify("peso muerto"), "peso_muerto");
        assert_eq!(slugify("press militar"), "press_militar");
        assert_eq!(slugify("!!!"), "user");
        assert_eq!(slugify(""), "user");
    }
}
