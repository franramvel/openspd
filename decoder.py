#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 OpenSPD contributors
"""
Decodificador en vivo del encoder VBT (1a gen, WiFi).

Protocolo descubierto por ingenieria inversa:
    El cliente se conecta por TCP al AP del encoder (192.168.4.1:80) y este EMITE
    lineas ASCII con este formato:

        @<seq>*<campo1>#<campo2>$<campo3>&\r\n

    p.ej.  @012*0.69#54.14$1.14&

    seq    = contador de muestra (incrementa +1)
    campo1 = hipotesis: velocidad media (m/s)
    campo2 = hipotesis: ROM/recorrido o potencia
    campo3 = hipotesis: velocidad pico (m/s)  (siempre > campo1)

Uso:
    python3 decoder.py            # se conecta y muestra los datos en vivo
    python3 decoder.py --raw      # muestra tambien la linea cruda
"""
import socket
import sys
import re
import argparse

HOST = "192.168.4.1"
PORT = 80

# @seq * c1 # c2 $ c3 &
LINE_RE = re.compile(rb"@(\d+)\*([-\d.]+)#([-\d.]+)\$([-\d.]+)&")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default=HOST)
    ap.add_argument("--port", type=int, default=PORT)
    ap.add_argument("--raw", action="store_true", help="mostrar linea cruda")
    args = ap.parse_args()

    print(f"Conectando a {args.host}:{args.port} ... (Ctrl-C para salir)")
    with socket.create_connection((args.host, args.port), timeout=10) as s:
        s.settimeout(60)
        print(f"{'seq':>5} | {'campo1':>8} | {'campo2':>8} | {'campo3':>8}"
              + ("   | crudo" if args.raw else ""))
        print("-" * (40 + (20 if args.raw else 0)))
        buf = b""
        while True:
            data = s.recv(1024)
            if not data:
                print("\n[conexion cerrada por el encoder]")
                break
            buf += data
            # procesar por lineas terminadas en \r\n
            while b"\r\n" in buf:
                line, buf = buf.split(b"\r\n", 1)
                line = line.strip()
                if not line:
                    continue
                m = LINE_RE.search(line)
                if m:
                    seq, c1, c2, c3 = (x.decode() for x in m.groups())
                    out = f"{seq:>5} | {c1:>8} | {c2:>8} | {c3:>8}"
                    if args.raw:
                        out += f"   | {line.decode(errors='replace')}"
                    print(out, flush=True)
                else:
                    # linea que no encaja con el patron conocido: mostrarla cruda
                    print(f"[?] desconocido: {line!r}", flush=True)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        print("\nadios")
    except Exception as e:
        print(f"\nERROR: {e}", file=sys.stderr)
        sys.exit(1)
