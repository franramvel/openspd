// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 OpenSPD contributors
//! Generador de bindings de uniffi. Ejemplos:
//!   cargo run -p openspd-ffi --bin uniffi-bindgen -- generate --library \
//!     target/debug/libopenspd_ffi.so --language kotlin --out-dir bindings/kotlin
//!   cargo run -p openspd-ffi --bin uniffi-bindgen -- generate --library \
//!     target/debug/libopenspd_ffi.so --language swift  --out-dir bindings/swift
fn main() {
    uniffi::uniffi_bindgen_main()
}
