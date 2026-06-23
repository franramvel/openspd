#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-or-later
# Copyright (C) 2026 OpenSPD contributors
"""
Metricas VBT (Velocity Based Training) calculadas a partir de las repeticiones.

Conceptos clave:
- Velocidad media (MV): la metrica principal para autorregular carga.
- Velocidad de la 1a rep: referencia para medir fatiga dentro de la serie.
- Velocity Loss (VL%): cuanto cae la velocidad respecto a la mejor rep de la serie.
  Es EL parametro de autorregulacion: p.ej. "parar la serie al perder 20%".
"""
from __future__ import annotations
from dataclasses import dataclass
from statistics import mean
from protocol import Rep


@dataclass
class SetSummary:
    n_reps: int
    best_mean_velocity: float    # mejor (mas rapida) velocidad media de la serie
    last_mean_velocity: float
    avg_mean_velocity: float
    peak_velocity: float         # pico maximo de toda la serie
    avg_rom: float
    velocity_loss_pct: float     # (mejor - ultima) / mejor * 100

    def __str__(self) -> str:
        return (
            "── Resumen de la serie ──────────────────────\n"
            f"  Reps:                 {self.n_reps}\n"
            f"  Vel. media (1a/mejor):{self.best_mean_velocity:5.2f} m/s\n"
            f"  Vel. media (ultima):  {self.last_mean_velocity:5.2f} m/s\n"
            f"  Vel. media (prom):    {self.avg_mean_velocity:5.2f} m/s\n"
            f"  Vel. pico (max):      {self.peak_velocity:5.2f} m/s\n"
            f"  ROM medio:            {self.avg_rom:6.2f} cm\n"
            f"  VELOCITY LOSS:        {self.velocity_loss_pct:5.1f} %\n"
            "─────────────────────────────────────────────"
        )


def velocity_loss(reps: list[Rep]) -> float:
    """% de perdida de velocidad respecto a la mejor rep de la serie."""
    if not reps:
        return 0.0
    best = max(r.mean_velocity for r in reps)
    if best <= 0:
        return 0.0
    return (best - reps[-1].mean_velocity) / best * 100.0


def summarize(reps: list[Rep]) -> SetSummary | None:
    if not reps:
        return None
    return SetSummary(
        n_reps=len(reps),
        best_mean_velocity=max(r.mean_velocity for r in reps),
        last_mean_velocity=reps[-1].mean_velocity,
        avg_mean_velocity=mean(r.mean_velocity for r in reps),
        peak_velocity=max(r.peak_velocity for r in reps),
        avg_rom=mean(r.rom for r in reps),
        velocity_loss_pct=velocity_loss(reps),
    )


# Zonas de velocidad orientativas (squat) para feedback de carga.
# Son aproximadas y dependen del ejercicio; ajustar con tu perfil carga-velocidad.
def load_zone(mean_velocity: float) -> str:
    if mean_velocity >= 1.0:
        return "Fuerza-velocidad / explosivo (carga ligera)"
    if mean_velocity >= 0.75:
        return "Fuerza-potencia (carga media)"
    if mean_velocity >= 0.5:
        return "Fuerza acelerada (carga media-alta)"
    return "Fuerza maxima (carga alta)"
