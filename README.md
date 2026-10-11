# SCS Telemetry Hub

An independent SCS Telemetry SDK plugin for American Truck Simulator and Euro Truck Simulator 2.
It reads live game telemetry and republishes it as plain JSON over a local HTTP server, instead of
the fixed-layout shared-memory struct most existing tools use.

*SCS Telemetry Hub is an independent, fan-made project. It is not affiliated with, endorsed by, or
sponsored by SCS Software. "American Truck Simulator" and "Euro Truck Simulator 2" are trademarks
of SCS Software. It reads telemetry the games publish through their own official SDK; nothing here
extracts, modifies, or redistributes any game asset.*

## Why

Existing plugins for this SDK publish data as a raw Memory Mapped File - fast, but every consumer
has to keep an exactly-matching binary struct definition in lockstep with the plugin. The most
widely used one, `RenCloud/scs-sdk-plugin`, has also been unmaintained since September 2023 and
predates several game features now in active use (ATS's Road Trip DLC "Quick Job" courier system
among them).

This project takes a different approach on both fronts: it's built on
[`scs-sdk-plugin`](https://github.com/AptS-1547/scs-sdk-crates), an actively-maintained Rust
framework over the official SDK, and it exposes data as plain JSON over HTTP rather than a binary
struct - trivially debuggable (`curl http://127.0.0.1:38471/snapshot`, or just open `/stream` in a
browser tab and watch it update live) and usable from any language without a matching struct
definition.

## Installing

Copy the built `scs_telemetry_hub.dll` into `bin/win_x64/plugins/` of your ATS or ETS2 install -
the same folder any other SDK telemetry plugin goes in. Create the folder if it doesn't exist yet.
It coexists fine with other telemetry plugins already installed there.

## What it publishes

The plugin starts a local HTTP server at `http://127.0.0.1:38471` (loopback only - it's a local
bridge for other processes on the same machine, never a network service) as soon as the game loads
it.

- **`GET /snapshot`** - the complete current state as one JSON object: live vehicle telemetry,
  static vehicle specification, active job, and H-shifter configuration.
- **`GET /stream`** - Server-Sent Events. Sends the current full state immediately on connect, then:
  - a `{"type":"snapshot", ...}` message roughly 10 times/second while driving,
  - a `{"type":"job", "data": {...}}` message the instant the active job changes (not on the next
    throttled tick),
  - a `{"type":"event", "data": {"name": "...", ...}}` message for gameplay events as they happen -
    `job_delivered`, `job_cancelled`, `player_fined`, `tollgate_paid`, `ferry_used`, `train_used`,
    `refuel_started`, `refuel_ended`, `refuel_paid`,
  - a `{"type":"disconnected"}` message if the plugin is shutting down, after which the stream
    ends and the port is closed.

### Snapshot shape (abbreviated)

```jsonc
{
  "type": "snapshot",
  "game": "ats",                    // "ats" | "ets2" | "unknown"
  "data": {
    "speed_mps": 0.0, "engine_rpm": 0.0, "engine_gear": 0, "displayed_gear": 0,
    "fuel_liters": 0.0, "fuel_average_consumption": 0.0, "adblue_liters": 0.0,
    "oil_pressure": 0.0, "water_temperature": 0.0, "battery_voltage": 0.0,
    "hshifter_slot": 0, "hshifter_selectors": [false, false],
    "placement": { "position": { "x": 0.0, "y": 0.0, "z": 0.0 }, "orientation": { "heading": 0.0 } },
    "truck_wheels": [ { "suspension_deflection": 0.0, "on_ground": true, "lift": 0.0, "...": "..." } ],
    "trailer": { "connected": false, "wheels": [ "..." ] },
    "wear_engine": 0.0, "wear_transmission": 0.0, "wear_cabin": 0.0, "wear_chassis": 0.0,
    "odometer_km": 0.0, "navigation_distance_m": 0.0, "navigation_speed_limit_mps": 0.0,
    "game_time_minutes": 0, "next_rest_stop_minutes": 0
  },
  "truck_spec": {
    "brand": "Kenworth", "name": "W900", "fuel_capacity_liters": 454.25, "rpm_limit": 2100.0,
    "shifter_type": "hshifter",                 // "arcade" | "automatic" | "manual" | "hshifter"
    "wheel_steerable": [true, false], "wheel_powered": [false, true], "wheel_liftable": [false]
  },
  "trailer_spec": { "name": "", "wheel_liftable": [] },
  "hshifter": { "selector_count": 2, "slots": [ { "slot": 0, "gear": 1, "selectors": 0 } ] },
  "job": {
    "active": true, "source_kind": "job",       // "job" | "car_job" | "bus_job"
    "cargo": "...", "cargo_mass_kg": 0.0,
    "source_city": "...", "destination_city": "...", "destination_city_id": "...",
    "source_company": "...", "destination_company": "...", "destination_company_id": "...",
    "income": 0, "planned_distance_km": 0, "delivery_time_seconds": 0,
    "job_market_raw": "quick_job", "job_market_known": "QuickJob"
  }
}
```

Channel, configuration, and event names throughout mirror the official SDK's own names
(`channels::truck::*`, `configuration::attributes::*`, `gameplay::events::*` in `plugin/src/lib.rs`)
rather than inventing new ones, so cross-referencing SCS's own SDK docs stays straightforward. See
that file for the exact, complete field list - this README shows the shape, not every field.

### Job types

ATS's Road Trip DLC added a personal-car "Quick Job" courier system that publishes under a
*different* configuration id (`car_job`) than classic freight jobs (`job`) - discovered and
handled here before SCS had even documented it publicly. `bus_job` is the same story for ETS2's
upcoming Coaches DLC - the id already exists and is handled, ready for whenever that DLC ships and
starts populating it. `job.source_kind` tells you which one a given job came from; the rest of the
shape is the same either way (car jobs simply leave cargo-related fields at their defaults, since
they genuinely have no cargo concept).

`job.delivery_time_seconds`, despite its name (kept as-is for wire compatibility), is **not** a
countdown - it's the SDK's own absolute deadline on the in-game clock, in game-minutes, the same
units and epoch as the top-level `game_time_minutes` channel. Minutes remaining is
`delivery_time_seconds - game_time_minutes`, not the field by itself.

### Refuel detection

There's no official SDK event for refueling at all. `refuel_started` / `refuel_ended` /
`refuel_paid` are inferred from `truck.fuel.amount` trending up/down/flat plus
`truck.engine.enabled` (fuel added, then the engine restarted) - the same heuristic the most
widely-used existing plugin uses internally, fired here as clean one-shot events rather than a
level state a consumer has to poll and edge-detect itself.

## Using it from your own app

No SDK, client library, or shared-memory struct required - it's just HTTP and SSE, so any language
with an HTTP client works.

**One-shot read (any language, e.g. a shell script):**
```bash
curl http://127.0.0.1:38471/snapshot
```

**Live stream in JavaScript (browser or Node 18+):**
```js
const stream = new EventSource("http://127.0.0.1:38471/stream");
stream.onmessage = (e) => {
  const msg = JSON.parse(e.data);
  if (msg.type === "snapshot") console.log(msg.data.speed_mps);
  if (msg.type === "event") console.log("event:", msg.data.name);
};
```

**Live stream in Python** (`pip install sseclient-py requests`):
```python
import json, requests, sseclient

resp = requests.get("http://127.0.0.1:38471/stream", stream=True)
for event in sseclient.SSEClient(resp):
    if not event.data:
        continue
    msg = json.loads(event.data)
    if msg["type"] == "snapshot":
        print(msg["data"]["speed_mps"])
```

Any SSE client library works the same way - there's nothing plugin-specific about the transport,
just the JSON shape documented above. The plugin only starts publishing once a game with it
installed is running; there's no need to launch it separately or manage its lifecycle.

## Building

Requires a Rust toolchain (`rustup`) and the MSVC C++ Build Tools workload (Visual Studio Build
Tools, "Desktop development with C++" workload) for the `x86_64-pc-windows-msvc` target.

```bash
cargo build --release
```

Produces `target/release/scs_telemetry_hub.dll`, exporting the two entry points
(`scs_telemetry_init`, `scs_telemetry_shutdown`) the SDK's plugin loader requires.

`cargo test` runs the unit tests, which start the real HTTP server on an ephemeral port - no game
needed.

## Status

Live-tested against real ATS and ETS2 sessions: vehicle telemetry, job data (including both
`job` and `car_job` sources), GPS-routing-relevant fields, and refuel detection all confirmed
working, on both a truck and a personal car, in both games. See `CHANGELOG.md` for the detailed
history, including real bugs found and fixed along the way.

## License

MIT - see `LICENSE`. See `THIRD_PARTY_NOTICES.md` for the licenses of everything this plugin is
built on.
