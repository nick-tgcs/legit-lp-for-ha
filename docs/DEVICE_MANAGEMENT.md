# Device management — add/edit/remove devices from the panel (v0.2.0)

**Status:** implemented · 2026-06
**Scope:** the add-on's registry layer + ingress panel — `config.rs`, `cycle.rs`,
`model.rs`, `lp.rs`, `web.rs`, `ha_client.rs`, `main.rs`, `assets/index.html`.

Until now the registry (`/config/legit_lp.yaml`) was hand-authored YAML, applied
only on restart. v0.2.0 makes devices **user-managed from the panel**: a devices
list + a 4-step add/edit wizard, persisted and applied live without a restart.

## What's new

### The panel is now a 3-view app
`dashboard` (the Friendly plan) · `devices` (management list) · `wizard`
(add/edit). The dashboard's device cards and schedule lanes are derived from the
same registry, so an add/edit/remove reflects on the plan immediately.

### The 5 device kinds (plain-language → scheduler concept)
| Wizard kind | Engine shape |
|---|---|
| Scheduled run | `planning: runtime` + `must_have: runtime` (deferrable hours-in-a-window) |
| Comfort range | `planning: immediate` + `must_have: temperature_band` |
| Keep under a limit | `planning: immediate` + `must_have: threshold` (`direction: below`/`above`) |
| Fixed program | `planning: runtime` + `must_have: program` (run-once contiguous block) |
| Battery | `global.storage[]` |

Two engine capabilities were added for full parity:
- **`threshold` at-or-above** — generalises the old `humidity_below` to a
  `Threshold { dir: Below|Above, limit }` (humidifier as well as dehumidifier).
  `humidity_below` still parses (→ `Below`), so existing registries are unchanged.
- **`program`** — a run-once contiguous block placed at the cheapest feasible
  start. Lowered to a `runtime` demand with `min_run` forced to the block length
  and a single start, so the whole run lands under any price cap or not at all
  (all-or-nothing). No new MILP — it reuses the min-up-time constraint.

The closed `hot_water|dehumidifier|aircon` device-type enum is **gone**; whether a
load's running state is read as a thermostat is derived from the entity domain
(`climate.*`), so any device kind works.

## How a save takes effect (no restart)
The registry is held behind a `watch` channel shared with the solve loop. A panel
edit:
1. builds the **full** device config (the wizard provides the explicit HA mapping),
2. `POST/PUT/DELETE /api/devices` → the whole registry is **validated** (the same
   validation the loader applies) and **atomically** written (temp file + rename),
3. published on the channel; the loop hot-swaps it and re-solves on the next tick.

A rejected edit returns `400 <message>` and never touches the live file or plan.

## API
- `GET  /api/devices` → `{ loads: [...], storage: [...] }` (the registry, as persisted).
- `POST /api/devices` — body `{ "type": "load"|"storage", "config": {…} }` (add).
- `PUT  /api/devices/{id}` — replace by id (edit; may rename).
- `DELETE /api/devices/{id}` — remove.
- `GET  /api/entities?domains=switch,sensor,climate,select` — live HA entity
  catalog for the wizard's entity picker (HA-unreachable → `502`, never fake data).

## Registry ownership + the no-hardcoding rule
The UI **owns** the whole registry file: every save re-serializes the entire
parsed struct, so fields the wizard doesn't surface (start/stop services,
`can_take`, per-direction battery control, predictive rates) survive untouched —
only hand-written comments are lost (the file is now machine-managed).

To keep the no-hardcoding contract intact, **every tunable numeric field in the
wizard is editable as either a literal or an `{ entity }` ref** (the literal⇄entity
toggle). Editing an existing device therefore preserves its live slider/sensor
bindings rather than freezing them to numbers — verified end-to-end (editing the
aircon keeps its `target_c` entity-ref, `predictive` planning, and `can_take`).

## Tests
TDD throughout; `make test` green (192 tests). Highlights:
- `config.rs`: serialize round-trip (lossless, comment-free), validate-before-save,
  atomic save, `threshold`/`program` parse + round-trip.
- `lp.rs`: `p1`/`p2` threshold at-or-above on/off; `p3`/`p4` program contiguous
  block + all-or-nothing under the price cap.
- `cycle.rs`: threshold-above + program resolve through a full cycle.
- `web.rs`: hot-swap commit (persist/publish/nudge + reject-without-persist), the
  full devices CRUD over HTTP, the entity catalog, and the Friendly panel + wizard
  served-asset contract.
- `ha_client.rs`: entity-catalog parse/filter/sort.

## Out of scope / follow-up
- Dark mode (palette is token-based; a later swap).
- Wizard validation is light (required entities, numeric bounds) — production could
  pre-check entity existence/domain before save.
- A new battery added via the wizard is advisory (no charge/discharge control
  block); live battery control stays a deliberate, separately-configured cutover.
