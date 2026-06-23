#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 OpenSPD contributors
"""
Protocolo del encoder VBT (1a gen, WiFi).

Descubierto por ingenieria inversa y CONFIRMADO contra la pantalla del encoder:

    El encoder es su propio AP WiFi. Te conectas por TCP a 192.168.4.1:80 y, sin pedir
    nada, EMITE una linea ASCII por CADA REPETICION completa, terminada en "\r\n":

        @<rep>*<vel_media>#<rom>$<vel_maxima>&

    ejemplo:  @4*1.55#55.77$2.06&
        rep         = 4         numero de repeticion (entero, incrementa)
        vel_media   = 1.55      velocidad media de la rep   (m/s)
        rom         = 55.77     recorrido / range of motion (cm)
        vel_maxima  = 2.06      velocidad pico de la rep    (m/s)  (siempre > media)
"""
from __future__ import annotations
from dataclasses import dataclass
import re

# AP del encoder
ENCODER_HOST = "192.168.4.1"
ENCODER_PORT = 80

# @rep * vel_media # rom $ vel_maxima &
_LINE_RE = re.compile(r"@(\d+)\*([-\d.]+)#([-\d.]+)\$([-\d.]+)&")


@dataclass(frozen=True)
class Rep:
    """Una repeticion decodificada del encoder."""
    rep: int            # numero de repeticion
    mean_velocity: float  # m/s
    rom: float            # cm
    peak_velocity: float  # m/s

    def __str__(self) -> str:
        return (f"rep {self.rep:>3} | media {self.mean_velocity:5.2f} m/s | "
                f"ROM {self.rom:6.2f} cm | pico {self.peak_velocity:5.2f} m/s")


def parse_line(line: str | bytes) -> Rep | None:
    """Convierte una linea cruda del encoder en un Rep. Devuelve None si no encaja."""
    if isinstance(line, bytes):
        line = line.decode("ascii", errors="replace")
    m = _LINE_RE.search(line)
    if not m:
        return None
    rep, mean_v, rom, peak_v = m.groups()
    return Rep(
        rep=int(rep),
        mean_velocity=float(mean_v),
        rom=float(rom),
        peak_velocity=float(peak_v),
    )


def iter_reps(byte_stream):
    """
    Generador: recibe trozos de bytes (p.ej. de socket.recv) via .send()/iteracion
    y va emitiendo Reps completas. Maneja el buffer y el separador \r\n.

    Uso tipico:
        buf = b""
        while True:
            data = sock.recv(1024)
            buf += data
            while b"\r\n" in buf:
                raw, buf = buf.split(b"\r\n", 1)
                rep = parse_line(raw)
                ...
    (la logica real de buffering vive en app.py; esto es solo documentacion del formato)
    """
    raise NotImplementedError("ver app.py para el manejo de buffer en vivo")


if __name__ == "__main__":
    # autotest con muestras reales capturadas
    samples = [
        "@3*0.26#45.95$0.38&",
        "@4*1.55#55.77$2.06&",
        "@12*0.69#54.14$1.14&",
        "basura sin formato",
    ]
    for s in samples:
        print(f"{s!r:30} -> {parse_line(s)}")
