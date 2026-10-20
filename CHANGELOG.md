# Changelog

## 0.1.3 (unreleased)

### Added
- `trailer_spec.body_type` and `trailer_spec.chain_type` - same ETS2 1.14/ATS 1.01+ version gate as
  `name`.
- `trailer_spec.wheel_positions` and `trailer_spec.wheel_radii` - per-wheel position (trailer-local
  space) and radius, giving the real axle layout (count, spacing, rear-most overhang). The SDK has
  no "trailer length" field at all; this is the closest proxy to it.

## 0.1.2

### Added
- `truck_spec.hook_position` and `trailer_spec.hook_position` - the 5th-wheel and kingpin coupling
  points, in each vehicle's own local space, decoded from the SDK's `hook.position` configuration
  attribute (existed in the SDK all along, just never read). Lets a consumer compute exact hitch
  alignment without surveying every vehicle by hand from coupled poses.
- `trailer_spec.id` - a trailer identifier that's always present, unlike `name`, which the SDK only
  sends from ETS2 1.14/ATS 1.01 onward.

### Fixed
- `trailer_spec` used to be overwritten with blank data whenever SCS sent an empty `trailer`
  configuration event - which happens both on a genuine detach and, confirmed live, as a spurious
  resend on a menu open or save reload while the exact same trailer is still physically hooked. An
  empty event no longer touches `trailer_spec`; it now only resets the live `data.trailer.connected`
  flag and `data.trailer.placement`, which the SDK otherwise leaves frozen at their last reading
  with no further signal that they've gone stale - live testing caught the hub reporting
  `connected=true` and a frozen placement for a trailer the truck had already driven 220 m away
  from.
- That same reset could get permanently stuck wrong: `trailer.connected` was subscribed with the
  SDK's default on-change-only delivery, so once a spurious empty config forced it to `false` while
  the trailer was still actually attached, nothing would ever re-send `true` again - SCS only
  re-sends a channel when its value changes, and from the SDK's side nothing had. Now subscribed
  with `EACH_FRAME` instead, so the real value is re-asserted every frame and a wrong forced reset
  lasts at most one frame.
- `/stream` could go silent for minutes after the game reset its render-time clock (a save,
  vehicle switch, or profile reload) - `maybe_broadcast_snapshot`'s throttle computed `now - last`
  with `saturating_sub`, which floors at 0 when the clock jumps backwards, so it kept concluding
  "too soon" until the new timeline climbed back past the old high-water mark. `/snapshot` stayed
  live throughout; only the periodic `snapshot` broadcasts stopped. Fixed by treating a backwards
  jump as due for a broadcast immediately, the same as a fresh start.
- `/stream` now writes and flushes every message by hand instead of going through tiny_http's
  buffered `Response`/`Read`-body path, which only flushed once the body reached EOF (fine for a
  response built in one go, not for a connection meant to stay open). A client connecting during a
  stall used to receive 0 bytes - not even the HTTP status line - until the next broadcast; it now
  gets headers immediately. The same rewrite adds a keepalive comment every 5s when nothing else is
  being broadcast, so a client whose socket died is now noticed (a failed write) within one
  keepalive interval instead of holding its connection slot indefinitely - previously only a real
  broadcast could expose a dead connection, which combined with the stall above to exhaust
  `MAX_HTTP_CONNECTIONS` during one live incident and start 503-ing every request, including
  `/snapshot`.
- `/snapshot` is now exempt from the `/stream` connection cap entirely (it completes on its own
  thread almost immediately and never holds a slot open), so a pile-up of long-lived `/stream`
  connections can no longer block a client's one-shot read.

## 0.1.1
- `job.delivery_time_seconds` was undocumented and the name is misleading - it's not a countdown,
  it's the SDK's absolute delivery deadline on the in-game clock, in game-minutes (same units/epoch
  as `game_time_minutes`). Minutes remaining is `delivery_time_seconds - game_time_minutes`. Wire
  name kept as-is for compatibility; this is a documentation fix, not a data change - a consumer
  that read it as a countdown would have gotten a constant, meaningless value.
- Shutdown now actually shuts down. `scs_telemetry_shutdown` used to send one `disconnected`
  message and nothing else - the HTTP server and every open `/stream` connection stayed alive
  until the game process itself died, so a client only learned the game had closed several
  seconds later, from a connection reset. It now ends every stream (each client still receives
  the `disconnected` message first) and closes the listening port, so a client retrying during
  the game's exit is refused instead of connecting to a server whose plugin is about to unload.
- The plugin module is now pinned in memory for the rest of the process's life. `tiny_http`
  keeps detached worker threads this plugin has no handle to (they only exit ~5 seconds after the
  server is dropped), so they can't be joined before the game unloads the DLL - and a thread that
  wakes inside unloaded code is an access violation at game exit. One such fault was logged
  against `scs_telemetry_hub.dll_unloaded`. Pinning makes the game's unload a no-op for this
  module. This is the plugin's only `unsafe` code: one `GetModuleHandleExW` call.
- No longer writes 112 `lookup_channel: index N is not valid` ERROR lines to the game log on
  every start. Wheel channels were subscribed for indices 0-23, but both games only accept truck
  indices 0-13 and trailer indices 0-19; subscribing is capped to exactly that. (It was harmless -
  the subscriptions are optional - but real errors were buried in the noise.)
- If the HTTP server can't start (port already in use, e.g. ATS and ETS2 running together), a
  warning is now written to the game log instead of failing silently.

### Added
- Unit tests that run the real HTTP server: the `/snapshot` endpoint, a stream ending cleanly
  with its port released on shutdown (both for a `Connection: close` client and for a keep-alive
  one, which learns the stream ended from the final chunk), the connection cap returning `503`
  and freeing its slots, and module pinning. Each shutdown behavior was checked by temporarily removing the fix and
  confirming the test fails.

## 0.1.0 (V1.0)

Initial public release.

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
