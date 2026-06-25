# OpenSPD — cliente abierto (ingeniería inversa) para encoders VBT

Cliente abierto y multi-encoder para encoders VBT de terceros, por ingeniería inversa para
interoperabilidad:
- **encoder v1** (WiFi): es su propio AP y transmite las repeticiones por un socket TCP en texto plano.
- **encoder v2** (BLE): stream cifrado (AES-128-ECB) por Bluetooth LE; ver §4.

OpenSPD es software independiente, sin relación ni respaldo de los fabricantes de los encoders.

**Requisitos:** toolchain de **Rust** (`cargo`). Probado en **Linux** (los comandos de red usan
NetworkManager/`nmcli`, y el encoder v2 usa BlueZ/`libdbus`). En Windows/macOS el bloque de `nmcli`
no aplica tal cual. El **GUI** emite beeps de cuenta atrás y una alarma de velocidad, por lo que en
Linux necesita las cabeceras de **ALSA** al compilar (`libasound2-dev` en Debian/Ubuntu, `alsa-lib`
en Arch); si no hay dispositivo de audio en ejecución, el GUI funciona en silencio.

**Dispositivos probados:** un encoder VBT de **1ª generación, solo WiFi** (el v1) y un encoder **BLE**
(el v2), ambos en posesión del autor del proyecto. ⚠️ Los modelos comerciales **actuales** (p. ej. el
"Force") son hardware distinto y casi seguro **otro protocolo**: no des por hecha la compatibilidad
con un equipo nuevo.

> ⚠️ **Seguridad / entrenamiento:** OpenSPD estima %1RM y avisa de fatiga (velocity loss) sobre los
> que vas a cargar peso real. Las estimaciones son **orientativas**, **no** sustituyen la supervisión
> de un profesional, y entrenas **bajo tu propia responsabilidad**. Sin garantía (ver `DISCLAIMER.md`).

## 1. Red: conectarse al encoder SIN perder internet (Linux + NetworkManager)

Ejemplo con un PC que tiene internet por cable (`enp5s0`) y WiFi (`wlan0`): se conecta el WiFi al
encoder pero se le impide tomar la ruta por defecto, así internet sigue saliendo por el cable
(ajusta los nombres de interfaz a los de tu equipo):

```bash
nmcli dev wifi connect "Speed4lifts_167" password "123456789" ifname wlan0
nmcli con modify "Speed4lifts_167" \
    ipv4.never-default yes ipv4.ignore-auto-dns yes \
    ipv6.never-default yes ipv6.ignore-auto-dns yes
nmcli con up "Speed4lifts_167"
```

> `Speed4lifts_167` es el **SSID de fábrica** que emite tu encoder (no es nombre de OpenSPD):
> hay que usarlo literal para que `nmcli` se conecte. Sustituye por el que muestre tu dispositivo.

Comprobar: `ip route` debe seguir mostrando la `default` por el cable; el encoder queda en
`192.168.4.1` y el PC recibe `192.168.4.x`.

## 2. Protocolo (decodificado y confirmado contra la pantalla del encoder)

> **Esto es el encoder v1, y aquí no hay nada cifrado: es texto plano.** El encoder usa el
> **puerto 80 pero NO habla HTTP** — es un **stream TCP propietario** que emite ASCII crudo. Un
> cliente HTTP normal fallará; hay que abrir un socket TCP y leer líneas. Para el v1, OpenSPD solo
> **decodifica/documenta** ese formato abierto: no existe ninguna medida tecnológica de protección
> que sortear.
>
> El **encoder v2 (BLE) es un caso distinto**: su stream sí va cifrado y, para interoperar, hay que
> descifrarlo (ver §4). Eso se hace **conscientemente al amparo de la excepción de ingeniería
> inversa para interoperabilidad** —sobre un dispositivo de tu propiedad, con software independiente
> y sin redistribuir nada del fabricante— según se detalla en §5. No lo escondemos: lo enmarcamos.

- Solo el **puerto 80/TCP** está abierto.
- No responde a HTTP/WebSocket. Nada más conectarte por TCP, **empuja** una línea ASCII
  por **cada repetición**, terminada en `\r\n`:

```
@<rep>*<vel_media>#<rom>$<vel_maxima>&
```

Ejemplo real `@4*1.55#55.77$2.06&`:

| Campo        | Símbolo | Ejemplo | Unidad | Pantalla del encoder |
|--------------|---------|---------|--------|----------------------|
| repetición   | `@`     | 4       | nº     | "repetición"         |
| velocidad media | `*`  | 1.55    | m/s    | "media"              |
| ROM          | `#`     | 55.77   | cm     | "ROM"                |
| velocidad máxima | `$` | 2.06    | m/s    | "máxima"             |
| fin          | `&`     | —       | —      | —                    |

Validado con 2 reps controladas: rep lenta/corta → `@3*0.26#45.95$0.38&`,
rep rápida/larga → `@4*1.55#55.77$2.06&` (velocidades suben, ROM sube, pico > media).

## 3. La app de PC — Rust (workspace `rust/`)

El crate está organizado como un **workspace** que separa el núcleo (lógica pura, reutilizable
incluso en móvil) del transporte y de la interfaz:

| Crate | Qué hace |
|-------|----------|
| `crates/core` (`openspd-core`) | Dominio **puro**: parser del protocolo, métricas VBT (velocity loss, resumen, %1RM, zonas), perfil carga-velocidad y cripto del v2. Sin red ni UI; con tests. |
| `crates/io` (`openspd-io`) | Transporte de escritorio: lectura TCP (v1) y BLE (v2) y persistencia CSV/perfil. |
| `crates/ffi` (`openspd-ffi`) | Bindings (uniffi) que exponen el core a **Kotlin (Android) y Swift (iOS)** para apps móviles nativas — ver [`crates/ffi/README.md`](./rust/crates/ffi/README.md). |
| raíz (`openspd-desktop`) | Los binarios de UI: `openspd` (CLI), `openspd-tui`, `openspd-gui`, `openspd-ble`. |

En móvil, la app nativa hace el TCP/BLE de la plataforma y delega en `openspd-core` el parsing, el
descifrado del v2, las métricas y el perfil; así la lógica vive en un solo sitio.

```bash
cd rust
cargo test --workspace        # tests (parser, métricas, perfil, cripto v2)
cargo build --release         # binarios en target/release/ (openspd, openspd-tui, …)
cargo run --release --bin openspd -- --exercise sentadilla --load 80 --vl-stop 20
```

Opciones: `--exercise banca|sentadilla` y `--load KG` (estima %1RM/1RM),
`--vl-stop PCT` (aviso de fatiga), `--rest SEG` (hueco para considerar nueva serie, def. 30),
`--csv ARCHIVO`. Cierra con **Ctrl‑C**; el CSV se guarda tras cada rep (nunca se pierde).

> Los `.py` de la raíz son la **referencia** del descubrimiento del protocolo (ya cumplieron).

### TUI (dashboard + constructor de perfil)

```bash
cargo run --release --bin openspd-tui -- --exercise sentadilla --load 40
```

Panel en vivo: serie actual, velocity loss (gauge), y el **perfil carga-velocidad** que se
ajusta solo. Atajos: `+/-` carga ±2.5 · `[ ]` ±10 · `c` cerrar serie · `u` deshacer punto ·
`s` guardar · `q` salir.

**Construir tu perfil:** pon una carga (`+/-`), haz la serie, descansa (o `c`) → se añade el
punto (carga, mejor velocidad). Repite con 2-3 cargas distintas → 1RM y R² en vivo. `s` guarda
`<ejercicio>.lvp`.

### GUI nativa (egui/eframe)

```bash
cargo run --release --bin openspd-gui -- --exercise sentadilla --load 40
cargo run --release --bin openspd-gui -- --profile sentadilla.lvp   # seguir un perfil
```

Ventana con: serie actual (tabla + gráfica de barras de velocidad + barra de velocity loss) y
panel de **perfil carga-velocidad** con scatter de puntos, recta de regresión y 1RM/R² en vivo.
Controles: combo de ejercicio, ± de carga, "Cerrar serie", "Deshacer punto", "Guardar".
Construye el perfil igual que el TUI (varias cargas → puntos → ajuste). Probada en NVIDIA+Wayland
(backend OpenGL/glow).

> ¿Por qué egui y no Tauri/WASM? El encoder habla **TCP crudo**; el sandbox del navegador (WASM)
> no abre sockets TCP, así que haría falta un backend nativo igual. egui es Rust nativo, reutiliza
> todo el crate, trae gráficas y no exige instalar webkit2gtk/node.

### Perfil carga-velocidad (`profile.rs`)

`v = a + b·carga` por mínimos cuadrados; con el umbral V1RM (0.30 squat, 0.17 banca, 0.50 peso
muerto) calcula tu **1RM real** y el **%1RM** de cualquier velocidad. Individual = mucho más
preciso que las ecuaciones poblacionales. Reúsalo en el CLI con `openspd --profile sentadilla.lvp`.

## 4. Encoder v2 (BLE, datos cifrados) — `openspd-ble`

OpenSPD también soporta un encoder **v2** que se comunica por **Bluetooth LE** con el stream
**cifrado (AES-128-ECB)**. El cliente genera una llave de sesión, la escribe en la característica
de desbloqueo nada más conectar (si no, el encoder corta la conexión a los ~3 s), y descifra las
repeticiones con los primeros 16 bytes de esa llave.

```bash
cargo run --release --bin openspd-ble                 # escanea y SELECTOR de encoders
cargo run --release --bin openspd-ble -- --address AA:BB:CC:DD:EE:FF
```

- **Selector**: escanea y lista los encoders disponibles (los v2 se detectan por su UUID de
  servicio); eliges uno por número. `--address` conecta directo.
- Cada repetición trae: nº, **fase concéntrica/excéntrica**, velocidad media propulsiva, ROM,
  velocidad pico, velocidad media y aceleraciones. Se reutilizan las mismas métricas (velocity
  loss, zonas) y se guarda CSV.
- Transporte vía **BlueZ (`bluer`)**. Módulo `encoderv2.rs` (llave + AES + parser) con tests que
  validan el descifrado sobre datos reales. Requiere `libdbus`/BlueZ (Linux).

> Nota: si el encoder v2 está conectado a otro central (p. ej. tu móvil), libéralo primero (apaga
> el Bluetooth del teléfono); BLE solo admite un central a la vez.

## 5. Licencia y aviso legal

OpenSPD se distribuye bajo la **GNU General Public License v3 o posterior** (`GPL-3.0-or-later`).
Texto completo en [`LICENSE`](./LICENSE).

**OpenSPD es software libre e independiente. NO es un producto oficial ni está afiliado, avalado o
relacionado con Speed4lifts ni con Vitruve.** Las marcas pertenecen a sus titulares y se citan solo
de forma nominativa para indicar compatibilidad. Las métricas (incl. %1RM/1RM) son orientativas y
sin garantía. Ver [`DISCLAIMER.md`](./DISCLAIMER.md).

**Encuadre: ingeniería inversa para interoperabilidad.** Este proyecto se desarrolla al amparo de
la excepción de interoperabilidad del derecho de autor —en México, el **artículo 114 Quáter,
fracción I**, de la Ley Federal del Derecho de Autor, con figuras análogas en otras jurisdicciones—,
sobre las siguientes bases:

- El dispositivo fue **adquirido legalmente y es propiedad** de quien lo usa.
- El **único propósito** es lograr la **interoperabilidad** de un programa **creado de forma
  independiente** (OpenSPD) con ese encoder.
- Es **ingeniería inversa de buena fe y sin ánimo de lucro**, realizada observando el comportamiento
  de un dispositivo propio.
- **No se redistribuye** firmware, claves ni código del fabricante: este repositorio contiene solo
  código original propio y la documentación del formato observado.

Lo anterior es una declaración de buena fe sobre el propósito del proyecto, **no asesoría legal**.
