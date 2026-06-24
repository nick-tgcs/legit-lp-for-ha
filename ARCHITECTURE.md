# Load Scheduling Architecture

## 1. Overview

Home Assistant owns the real device controls and sensors. The scheduler does not
directly understand specific device brands or integrations. It consumes load
declarations and produces scheduler decisions.

The scheduler is a pure decision engine. It reads state from Home Assistant,
applies rules, solves for an optimal schedule, and writes back only the current
timestep's actions through Home Assistant's own service API.

### Design rule: nothing operational is hardcoded in the engine

The engine holds **structural mechanics only** (solver, grid resolution, the schema
of a load, tie-break epsilons). It holds **no operational magnitude** — every price,
window, runtime, power, SoC limit, efficiency and wear cost comes from the registry,
and there are **no `default_*` value functions**: every operational config field is
*required* (a missing key is a hard parse error). When a live entity-ref can't be read
this cycle the engine **fails loud + safe** (an actionable diagnostic + a conservative
hold/skip), and never substitutes an invented number. This is enforced by the
`config.rs` `guard_*` tests. Full statement + the registry side of the rule:
`docs/lp-no-hardcoding.md` in the consuming HA-config repo.

## 2. Load Declaration

Each schedulable load declares:

| Field | Purpose |
|---|---|
| `start_control` | How the scheduler may start the load (HA service/entity) |
| `stop_control` | How the scheduler may stop the load (HA service/entity) |
| `set_rate_control` | How the scheduler may set its rate, if supported (HA service/entity) |
| `state_sensor` | What state/progress the scheduler can observe (HA entity) |
| `authority_entity` | Whether the scheduler currently has authority to control it (HA boolean entity) |
| `capability` | Physical capability (power range, runtime range, ramp profile) |
| `hard_rules` | Hard device rules this load imposes on scheduler actions |
| `must_have` | Required work the load demands |
| `can_take` | Optional appetite the load declares |
| `preferences` | Soft preferences for ranking legal schedules |

A load declaration is data, not code. It contains no arbitrary programming logic.

## 3. Authority/Rule Precedence

Precedence is strict. A lower-numbered layer always overrides a higher-numbered
layer. There are no exceptions, penalties, or fallback logic on hard rules.

```
┌─────────────────────────────────────────────┐
│ 1. Manual authority                          │
│    Human control wins.                      │
│    Scheduler authority disabled → must not  │
│    control. May still observe.               │
├─────────────────────────────────────────────┤
│ 2. General hard rules                       │
│    System-wide constraints.                 │
│    Absolute. Remove illegal actions.        │
│    No penalties, no exceptions.             │
├─────────────────────────────────────────────┤
│ 3. Load hard rules                          │
│    Device-specific constraints.             │
│    Absolute. Further restrict legal space.  │
│    Cannot override general hard rules.      │
├─────────────────────────────────────────────┤
│ 4. Must-have demand                         │
│    Required work.                           │
│    Scheduled only inside legal space.       │
│    If infeasible → report, do not violate   │
│    hard rules.                              │
├─────────────────────────────────────────────┤
│ 5. Can-take demand                          │
│    Optional appetite.                       │
│    Scheduled after must-have, inside legal  │
│    space. Must be capped.                   │
├─────────────────────────────────────────────┤
│ 6. Preferences                              │
│    Soft ranking only.                       │
│    Choose between legal schedules.          │
│    Never make illegal actions legal.        │
└─────────────────────────────────────────────┘
```

### Key distinctions

- **Authority** decides *whether* the scheduler may control the load.
- **Hard rules** decide *whether* a scheduler action is legal.
- **Must-have** defines *required* work.
- **Can-take** defines *optional* work.
- **Preferences** rank *legal* options.

## 4. Runtime Pipeline

The scheduler executes this pipeline every solve cycle:

```
 1. Read Home Assistant state
 2. Load global policy
 3. Load all schedulable load declarations
 4. Check scheduler authority per load
 5. Observe all loads, even those not under scheduler authority
 6. Generate candidate actions only for scheduler-authorised loads
 7. Apply general hard rules → remove illegal actions
 8. Apply load hard rules → remove illegal actions
 9. Schedule must-have demand using only legal actions
10. Schedule can-take demand using only legal remaining capacity
11. Use soft preferences only to rank legal schedules
12. Report infeasible or partially satisfied demand when needed
13. Execute only the current timestep through HA start/stop/set-rate
14. Publish status and explanations back to Home Assistant
```

Step 13 is critical: the solver may plan a full horizon, but only the
current timestep's decision is written to Home Assistant entities. Future
timesteps are replanned on the next cycle (Model Predictive Control).

Step 5 is equally critical: the scheduler observes all loads regardless of
authority. A load under manual control still reports its state and progress
so the scheduler can account for its energy impact.

Optionally (see §5 "Preview (shadow) planning"), step 6 can additionally produce
a *non-executable* shadow plan for observe-only loads so the panel can show what
the scheduler would do; those plans are never written to Home Assistant.

## 5. Authority Model

Authority is a boolean per load, read from a Home Assistant entity at runtime.

| Authority state | Scheduler behaviour |
|---|---|
| `on` | Scheduler may generate candidate actions for this load |
| `off` | Scheduler must not control this load; may still observe state and progress |

There is no `manual_on` or `manual_off` scheduler mode. Authority is binary:
the scheduler either has permission to act or it does not. When a human wants
direct control, they disable scheduler authority for that load and operate the
device through Home Assistant's normal UI or automations.

### Preview (shadow) planning

The scheduler may *solve* observe-only loads without *controlling* them. When
preview is on, authority-off loads with a known running state are included in the
optimisation so the panel can show what the scheduler would do — a diagnostic-only
shadow plan for testing and sampling.

Preview has two independent toggles, OR-combined (either on ⇒ on):

- the **in-panel checkbox** ("solve observe-only (preview)"), which POSTs
  `/api/preview` to a runtime flag the solve loop reads each tick. It is the
  quick way to sample a plan and is *not* persisted across a restart; and
- an optional **`preview_entity`** (an HA boolean), the persistent, automatable
  path — useful for leaving preview on, or driving it from an HA automation.

This does not weaken authority. Preview changes only which loads are *solved*, not
which are *controlled*: an unauthorised load's current-step decision is always
`NoChange`, and the executor refuses to act on any load without authority. No
service call is ever issued for a preview load — in dry-run or live. Preview is
the read-only complement to step 5 ("observe all loads") and §10 ("planned
actions ... for diagnostics only"); it never makes an illegal action legal.

## 6. Hard Rules

Hard rules are absolute constraints. They remove illegal actions from the
candidate set before demand is scheduled.

### General hard rules

System-wide rules that apply to all scheduler decisions. Examples:

- Battery SOC must not fall below the configured backup reserve.
- Grid export must not occur during configured peak windows.
- Total site power must not exceed connection limits.

### Load hard rules

Device-specific rules that further restrict what the scheduler may do with a
particular load. Examples:

- Dehumidifier must not run during peak windows.
- Hot water heater must complete its ramp-up before steady-state is counted.
- A load's power draw must not exceed its rated capacity.

Hard rules have no exceptions, no penalties, and no fallback logic. If a
must-have demand cannot be met within the legal space, the scheduler reports
infeasibility or partial satisfaction — it does not violate hard rules to
meet demand.

## 7. Demand

### Must-have

Required work declared by the load. The scheduler must attempt to satisfy it
within the legal space left by authority and hard rules.

If must-have demand cannot be fully scheduled, the scheduler reports:
- Which loads are partially satisfied.
- How much demand is unmet.
- Which hard rules prevented full satisfaction.

It does not relax hard rules to meet demand.

### Can-take

Optional appetite declared by the load. The scheduler may schedule it only
after must-have is addressed, only inside legal space, and it must be capped
at the declared appetite.

Can-take demand that cannot be scheduled is simply not scheduled. No
infeasibility report is needed.

## 8. Preferences

Preferences are soft. They rank legal schedules against each other. They
never make an illegal action legal.

Examples:

- Prefer cheaper intervals over more expensive ones.
- Prefer earlier completion over later completion (small delay penalty).
- Prefer solar-powered intervals over grid-powered intervals.

Preferences are expressed as objective function coefficients or tie-breaking
penalties in the optimisation. They influence which legal schedule is chosen,
not whether an action is legal.

## 9. Execution Model

The scheduler uses Model Predictive Control (MPC):

1. Solve the full planning horizon (typically 24 hours).
2. Execute only the current timestep's decision.
3. Re-solve on the next cycle with updated state.

The scheduler writes to Home Assistant through its own service API:
- `start_control` to turn a load on.
- `stop_control` to turn a load off.
- `set_rate_control` to adjust a load's power level.

The scheduler does not push a full day of future Home Assistant automations.
Future actions are planned but not pre-committed; they are replanned each cycle.

## 10. Status Reporting

After each cycle, the scheduler publishes back to Home Assistant:

- Current action being executed.
- Planned actions for the full horizon (for diagnostics only).
- Per-load status: running, idle, infeasible, partially satisfied.
- Authority state per load.
- Which hard rules are binding.
- Must-have satisfaction: met, partial, infeasible.
- Can-take utilisation.
- Cost and energy metrics.

## 11. Explicit Prohibitions

The following patterns are forbidden in this architecture:

| Prohibition | Reason |
|---|---|
| `manual_on` / `manual_off` scheduler modes | Authority is binary; there is no "manual" scheduler mode |
| Hard rules with exceptions | Hard rules are absolute by definition |
| Penalties on hard rules | Hard rules remove actions; they do not add cost |
| Load wants overriding general hard rules | Load hard rules further restrict; they never relax |
| Scheduler code that directly depends on device brands | The scheduler consumes declarations, not device APIs |
| Pushing a full day of future HA automations | MPC: execute current timestep only, replan next cycle |
| Arbitrary programming logic in load declarations | Declarations are data, not code |