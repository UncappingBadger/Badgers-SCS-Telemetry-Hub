# Changelog

## Unreleased

Initial development. Not yet tagged or published - see the project's own notes for release
readiness criteria before cutting a first version.

### Added
- Local HTTP server (`127.0.0.1:38471`) publishing live telemetry as JSON - `GET /snapshot` for a
  one-shot read, `GET /stream` for Server-Sent Events - instead of a shared-memory struct.
- Full vehicle telemetry: speed, RPM, gear (H-shifter and automatic), cruise control, all brake/
  light/warning channels, fuel and AdBlue (where the vehicle has it), wear, odometer, navigation
  distance/time/speed-limit, live per-wheel state (deflection/lift/on-ground/rotation, indexed) for
  both truck and trailer, and static per-wheel capability flags (steerable/powered/liftable).
  Common channels (`game.time`, `rest.stop`, multiplayer time offset) and the live H-shifter slot/
  selector channels plus the `hshifter` configuration's static slot-to-gear mapping.
- Job data covering all three known job-configuration sources: classic freight (`job`), ATS Road
  Trip DLC's personal-car Quick Jobs (`car_job`), and ETS2's upcoming Coaches DLC (`bus_job`, id
  handled ready for when that DLC ships and starts populating it) - discovered live before SCS had
  documented `car_job`/`bus_job` publicly at all, since they don't appear in the SDK 1.14 header
  snapshot the underlying framework audits (confirmed later against the real SDK 1.15 headers,
  which do document them).
- Gameplay events: job delivered/cancelled (both classic and car-job variants), player fined,
  tollgate paid, ferry/train used.
- Refuel detection (`refuel_started`/`refuel_ended`/`refuel_paid`) - the official SDK has no event
  for this at all; ported the same tick-based fuel-delta heuristic the most widely-used existing
  plugin uses internally, fired here as one-shot events instead of a polled boolean.
- Vehicle static specification (brand/model/id/license plate, fuel/AdBlue capacity, RPM limit,
  gear counts, shifter type) and trailer name.

### Fixed
- ATS fires the `job`/`car_job`/`bus_job` configuration events together as one batch whenever job
  state changes, with only one of the three ever carrying real data - naively applying every event
  in arrival order let a trailing empty one overwrite a genuine active job. Fixed by only clearing
  job state when the empty event matches the currently-active job's own source, not any of the
  other two.
- `car_job`'s market attribute is named `car_job.market`, not `job.market` (confirmed against the
  real SDK 1.15 headers) - was silently reading the wrong name and always getting an empty value.
- `car_job.delivered`/`car_job.cancelled` gameplay events were being silently dropped (matched only
  by typed SDK 1.14 constants, no raw-id fallback for the newer car-job variants) - added the same
  fallback pattern already used for configuration events.
- AdBlue channels are gated to ETS2 only (ATS doesn't expose them at all), but even within ETS2 a
  specific vehicle without an AdBlue system fitted doesn't register
  `truck.adblue.consumption.average` at all - a hard `subscribe` there crashed the whole plugin's
  initialization. Switched to `subscribe_optional`, which SCS's SDK is specifically designed to
  support for exactly this "not every vehicle has this" case.

### Maintenance
- Bounded the HTTP server's per-connection thread spawning (`MAX_HTTP_CONNECTIONS`) after a
  crash-safety audit, as a precautionary hardening measure - not a reproduced issue, but the server
  runs inside the game's own process and an unbounded client could otherwise exhaust threads there.
