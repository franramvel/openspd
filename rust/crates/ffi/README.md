# openspd-ffi — bindings móviles (Kotlin / Swift)

Expone el núcleo VBT de [`openspd-core`](../core) a **Android (Kotlin)** e **iOS (Swift)** mediante
[uniffi](https://mozilla.github.io/uniffi-rs/). El core es lógica pura (protocolo, métricas, perfil
carga-velocidad y cripto del encoder v2); este crate solo añade la capa de bindings.

## Reparto de responsabilidades en móvil

El **networking lo hace la app nativa** (no se cruza por FFI):

- **v1 (WiFi/TCP):** la app abre el socket y, por cada línea recibida, llama `parseLine(line)`.
- **v2 (BLE):** la app hace el escaneo/conexión nativos (Android BLE / CoreBluetooth), escribe en la
  característica de desbloqueo los `unlockBytes` de `generateKey()`, y por cada notificación cifrada
  llama `Reassembler.push(bytes)` → lista de `EncoderV2Rep` ya descifradas y parseadas.
- **Métricas y perfil:** `velocityLoss`, `summarize`, `est1rmPct/est1rmKg`, `lvpFit`, y los métodos
  de `Lvp` (`pct1rm`, `loadForVelocity`, `velocityForPct`, `toText`/`lvpFromText` para persistir).

## Generar los bindings

Los bindings ya generados están en `bindings/` (regenéralos tras cambiar la API):

```sh
# desde rust/
cargo build -p openspd-ffi
cargo run -p openspd-ffi --bin uniffi-bindgen -- generate \
  --library target/debug/libopenspd_ffi.so --language kotlin --out-dir crates/ffi/bindings/kotlin
cargo run -p openspd-ffi --bin uniffi-bindgen -- generate \
  --library target/debug/libopenspd_ffi.so --language swift  --out-dir crates/ffi/bindings/swift
```

## Compilar la librería nativa

### Android (con [cargo-ndk](https://github.com/bbqsrc/cargo-ndk))

```sh
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 -o ../android/app/src/main/jniLibs \
  build -p openspd-ffi --release
```

Copia `bindings/kotlin/uniffi/openspd_ffi/openspd_ffi.kt` al `src/main/java` de la app. La librería
JNA (`net.java.dev.jna:jna` con clasificador `@aar`) es requisito en tiempo de ejecución.

### iOS (XCFramework con el `staticlib`)

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim
cargo build -p openspd-ffi --release --target aarch64-apple-ios
cargo build -p openspd-ffi --release --target aarch64-apple-ios-sim
xcodebuild -create-xcframework \
  -library target/aarch64-apple-ios/release/libopenspd_ffi.a \
  -library target/aarch64-apple-ios-sim/release/libopenspd_ffi.a \
  -output OpenspdFfi.xcframework
```

Añade `bindings/swift/openspd_ffi.swift` y el `openspd_ffiFFI.modulemap` al target de Xcode.

## Tipos expuestos

- Records: `Rep`, `SetSummary`, `EncoderV2Rep`, `Point`, `SessionKey`; enum `Phase`.
- Objetos (con estado): `Reassembler` (BLE v2) y `Lvp` (perfil; creado con `lvpFit`/`lvpFromText`).
- Funciones: `parseLine`, `velocityLoss`, `summarize`, `est1rmPct`, `est1rmKg`, `loadZone`,
  `generateKey`, `parseRepetition`, `encoderV2ToRep`, `beginCommand`, `defaultV1rm`.
