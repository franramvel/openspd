#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 OpenSPD contributors
"""
App de PC para el encoder VBT (1a gen, WiFi).

- Se conecta al encoder (TCP 192.168.4.1:80).
- Decodifica cada repeticion en vivo.
- Muestra velocidad media/pico, ROM y VELOCITY LOSS acumulado.
- Al salir (Ctrl-C) guarda la serie en un CSV con fecha/hora.

Uso:
    python3 app.py                      # graba una serie, Ctrl-C para terminar
    python3 app.py --vl-stop 20         # avisa cuando el velocity loss supere 20%
    python3 app.py --csv mi_serie.csv   # nombre de archivo concreto

Antes de correr, conecta tu WiFi al AP 'Speed4lifts_167' (ver README).
"""
from __future__ import annotations
import argparse
import csv
import socket
import sys
import time

from protocol import ENCODER_HOST, ENCODER_PORT, parse_line, Rep
from metrics import summarize, velocity_loss, load_zone


def stream_reps(host: str, port: int):
    """Generador que conecta al encoder y emite Reps segun van llegando."""
    print(f"Conectando a {host}:{port} ... (Ctrl-C para terminar la serie)")
    with socket.create_connection((host, port), timeout=10) as s:
        s.settimeout(300)
        print("Conectado. Empieza a entrenar.\n")
        buf = b""
        while True:
            data = s.recv(1024)
            if not data:
                print("\n[el encoder cerro la conexion]")
                return
            buf += data
            while b"\r\n" in buf:
                raw, buf = buf.split(b"\r\n", 1)
                rep = parse_line(raw)
                if rep is not None:
                    yield rep
                elif raw.strip():
                    print(f"[?] linea no reconocida: {raw!r}", flush=True)


def save_csv(path: str, reps: list[Rep]) -> None:
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["rep", "vel_media_mps", "rom_cm", "vel_maxima_mps"])
        for r in reps:
            w.writerow([r.rep, r.mean_velocity, r.rom, r.peak_velocity])
    print(f"\nSerie guardada en: {path}")


def main() -> int:
    ap = argparse.ArgumentParser(description="Grabadora VBT encoder VBT")
    ap.add_argument("--host", default=ENCODER_HOST)
    ap.add_argument("--port", type=int, default=ENCODER_PORT)
    ap.add_argument("--vl-stop", type=float, default=None,
                    help="avisar al superar este %% de velocity loss")
    ap.add_argument("--csv", default=None, help="ruta del CSV de salida")
    args = ap.parse_args()

    reps: list[Rep] = []
    print(f"{'REP':>4} | {'MEDIA':>6} | {'ROM':>7} | {'PICO':>6} | {'VL%':>5} | zona")
    print("-" * 70)
    try:
        for rep in stream_reps(args.host, args.port):
            reps.append(rep)
            vl = velocity_loss(reps)
            line = (f"{rep.rep:>4} | {rep.mean_velocity:>6.2f} | {rep.rom:>7.2f} | "
                    f"{rep.peak_velocity:>6.2f} | {vl:>4.1f}% | {load_zone(rep.mean_velocity)}")
            print(line, flush=True)
            if args.vl_stop is not None and vl >= args.vl_stop:
                print(f"  ⚠️  VELOCITY LOSS {vl:.1f}% ≥ {args.vl_stop:.0f}%  → considera parar la serie",
                      flush=True)
    except KeyboardInterrupt:
        pass
    except OSError as e:
        print(f"\nError de red: {e}  (¿WiFi conectado al encoder?)", file=sys.stderr)
        return 1

    print()
    s = summarize(reps)
    if s:
        print(s)
        path = args.csv or f"serie_{int(time.time())}.csv"
        save_csv(path, reps)
    else:
        print("No se registró ninguna repetición.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
