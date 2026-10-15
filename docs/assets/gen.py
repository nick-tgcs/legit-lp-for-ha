#!/usr/bin/env python3
"""Generate the README demo SolveReport + horizon SVG.

The screenshots in docs/ are renders of the *real* panel frontend
(assets/index.html, byte-for-byte) driven by a committed, representative
SolveReport — so they regenerate deterministically when the UI changes, with
no live HA needed. The horizon SVG is a faithful port of the server-side
renderer in scheduler/src/web.rs (load rows of planned on-blocks; can-take
credit in green). Numbers mirror the reference Amber/Sonnen installation.

Run via make_screenshots.sh.
"""
import json
import pathlib

HERE = pathlib.Path(__file__).parent
STEP_MIN = 15
N = 24 * 60 // STEP_MIN  # 96 quarter-hour steps


def idx(hhmm: str) -> int:
    h, m = map(int, hhmm.split(":"))
    return (h * 60 + m) // STEP_MIN


def span(start: str, end: str) -> range:
    return range(idx(start), idx(end))


def blocks(*ranges) -> list[bool]:
    on = [False] * N
    for r in ranges:
        for t in r:
            on[t] = True
    return on


# --- the representative report -------------------------------------------
report = {
    "at": "2026-06-11T14:02:31+10:00",
    "solver_ms": 38,
    "dry_run": True,
    "global_enabled": True,
    "price_now": 0.11,
    "pv_now": 3.2,
    "consumption_now": 1.1,
    "grid": [f"{(t * STEP_MIN) // 60:02d}:{(t * STEP_MIN) % 60:02d}" for t in range(N)],
    "loads": [
        {
            "id": "hot_water",
            "planning": "runtime",
            "authority": True,
            "running": False,
            "action": "NoChange",
            "reason": "off now; must-have 45 min planned into 02:00–02:45 "
                      "(cheapest legal steps); can-take 60 min queued midday on solar",
            "unmet": 0.0,
            "executed": False,
            "on": blocks(span("02:00", "02:45"), span("11:00", "12:00")),
            "ct": blocks(span("11:00", "12:00")),
        },
        {
            "id": "dehumidifier",
            "planning": "immediate",
            "authority": True,
            "running": True,
            "action": "Start",
            "reason": "running; humidity 68 > 65 (+2 hysteresis cleared), "
                      "price 0.11 ≤ max 0.15",
            "unmet": 0.0,
            "executed": False,
            "on": blocks(span("14:00", "14:30")),
            "ct": [False] * N,
        },
        {
            "id": "aircon",
            "planning": "predictive",
            "authority": True,
            "running": False,
            "action": "NoChange",
            "reason": "hold; pre-cool 13:00–14:00 on surplus, must-have band "
                      "18:00–19:00; 15 min unmet (price > 0.20 in every legal step)",
            "unmet": 15.0,
            "executed": False,
            "on": blocks(span("13:00", "14:00"), span("18:00", "19:00")),
            "ct": blocks(span("13:00", "14:00")),
        },
    ],
    "diagnostics": [
        "forecast 4m old · 96 slots",
        "recorder ok",
        "config valid (3 loads)",
        "learned profile coverage 80% (warming up)",
    ],
}


def horizon_svg(r: dict) -> str:
    """Faithful port of scheduler/src/web.rs::horizon."""
    loads = r["loads"]
    n = max(len(r["grid"]), 1)
    row_h, w = 22, 1000
    h = 30 + row_h * max(len(loads), 1)
    svg = [f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" '
           f'font-family="Roboto,sans-serif" font-size="11">']
    for li, l in enumerate(loads):
        y = 20 + li * row_h
        svg.append(f'<text x="0" y="{y + 12}" fill="#888">{l["id"]}</text>')
        for t, on in enumerate(l["on"]):
            if on:
                x = 120.0 + (t / n) * (w - 120.0)
                bw = (w - 120.0) / n
                color = "#4caf50" if (t < len(l["ct"]) and l["ct"][t]) else "#03a9f4"
                svg.append(f'<rect x="{x:.1f}" y="{y}" width="{bw:.1f}" '
                           f'height="16" fill="{color}"/>')
    svg.append("</svg>")
    return "".join(svg)


(HERE / "demo-report.json").write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
(HERE / "horizon.svg").write_text(horizon_svg(report) + "\n")
print("wrote demo-report.json + horizon.svg")
