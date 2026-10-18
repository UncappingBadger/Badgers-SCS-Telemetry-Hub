//! Independent SCS Telemetry SDK plugin for ETS2/ATS.
//!
//! Reads live telemetry via `scs-sdk-plugin` (an actively-maintained, safe Rust
//! framework over SCS Software's official Telemetry SDK 1.14 - see
//! https://github.com/AptS-1547/scs-sdk-crates) and republishes it as plain JSON
//! over a local HTTP server, instead of a fixed-layout shared-memory struct.
//! That deliberately decouples every future consumer from a binary ABI: any
//! language/tool can read `GET /snapshot` or subscribe to `GET /stream`
//! (Server-Sent Events) without needing a matching struct definition.
//!
//! Callbacks below run synchronously on the game's own telemetry thread (see
//! the upstream example's own note on this), so nothing here may block. All
//! callback work is therefore limited to cheap in-memory updates and a
//! non-blocking `mpsc::Sender::send` to broadcast to already-connected HTTP
//! clients; the actual HTTP server runs on its own thread, spawned once.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use scs_sdk_plugin::sdk::{
    Attribute, ChannelFlags, FVector, GameSchemaAvailability, StringValue, TelemetryApiVersion,
    channels, configuration, game, gameplay,
};
use scs_sdk_plugin::{
    ChannelUpdate, ConfigurationEvent, Game, GameCompatibility, GameplayEvent, PluginCompatibility,
    PluginContext, PluginMetadata, PluginResult, TelemetryEvent, TelemetryEventKind,
    TelemetryPlugin, export_plugin,
};
use serde::Serialize;

/// Loopback-only by design - this is a local bridge for other processes on the
/// same machine (a future TruckHub, a debug curl session, etc.), never a
/// network service.
const LISTEN_ADDRESS: &str = "127.0.0.1:38471";

/// Snapshot broadcasts are throttled to this interval so a live `/stream`
/// consumer gets smooth updates without flooding on every single frame -
/// matches roughly what a HUD actually needs to redraw at, well under what a
/// human can perceive as choppy.
const SNAPSHOT_INTERVAL_MICROSECONDS: u64 = 100_000; // 10 Hz

/// How often `/stream` sends a keepalive comment when nothing else is being
/// broadcast. Without this, a stalled or paused session leaves a client
/// unable to tell whether it's even connected (see `serve_stream`), and a
/// client whose socket has died silently holds its connection slot forever
/// instead of being noticed and reaped.
const STREAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(5);

static SUPPORTED_GAMES: [GameCompatibility; 2] = [
    GameCompatibility::new(Game::EuroTruckSimulator2, game::ets2::V1_00),
    GameCompatibility::new(Game::AmericanTruckSimulator, game::ats::V1_00),
];

#[derive(Serialize, Default, Clone, Copy, Debug)]
struct Vec3 {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Serialize, Default, Clone, Copy, Debug)]
struct Orientation {
    heading: f32,
    pitch: f32,
    roll: f32,
}

#[derive(Serialize, Default, Clone, Copy, Debug)]
struct Placement {
    position: Vec3,
    orientation: Orientation,
}

/// A fixed point in a vehicle's own local space - deliberately a different
/// type from `Vec3`, which is always world space and changes every frame.
/// Matches the SDK's own `FVector` axis convention (vehicle-local: +X right,
/// +Y up, +Z backwards), kept here only because `FVector` itself has no
/// `Serialize` impl.
#[derive(Serialize, Default, Clone, Copy, Debug)]
struct LocalPoint {
    x: f32,
    y: f32,
    z: f32,
}

impl From<FVector> for LocalPoint {
    fn from(v: FVector) -> Self {
        Self { x: v.x, y: v.y, z: v.z }
    }
}

/// Everything read from the `truck` static-specs configuration - fetched once
/// (re-sent by SCS whenever the truck/trailer setup changes) rather than
/// polled every frame, since none of it changes mid-drive.
#[derive(Serialize, Default, Clone, Debug)]
struct TruckSpec {
    brand_id: String,
    brand: String,
    id: String,
    name: String,
    license_plate: String,
    license_plate_country: String,
    fuel_capacity_liters: f32,
    fuel_warning_factor: f32,
    adblue_capacity_liters: f32,
    adblue_warning_factor: f32,
    rpm_limit: f32,
    forward_gear_count: u32,
    reverse_gear_count: u32,
    retarder_step_count: u32,
    /// The 5th-wheel coupling point, in the truck's own local space - lets a
    /// consumer compute exact hitch/kingpin alignment without surveying each
    /// vehicle by hand from coupled poses.
    hook_position: LocalPoint,
    /// Static per-wheel capability flags - distinct from the live per-wheel
    /// state in `Snapshot.truck_wheels`. Sized to whatever `WHEEL_COUNT`
    /// actually reports, not `MAX_WHEEL_INDEX` - TruckHub's EXT suspension
    /// readout uses these to label an axle (fixed tag vs. drive vs. steer)
    /// rather than to plot a live reading.
    wheel_steerable: Vec<bool>,
    wheel_powered: Vec<bool>,
    wheel_liftable: Vec<bool>,
    /// From the separate `controls` configuration, not `truck` - merged into
    /// this struct anyway since it's static per-vehicle info like everything
    /// else here. One of "arcade"/"automatic"/"manual"/"hshifter" - kept as
    /// the raw SDK string rather than only a parsed enum, same reasoning as
    /// `JobInfo::job_market_raw`.
    shifter_type: String,
}

/// Static per-trailer info from the `trailer` configuration - name and
/// per-wheel liftable flags, the trailer-side counterparts to `TruckSpec`'s
/// fields above (trailers have no steerable/powered wheels at all, so there's
/// nothing to decode for those two here).
#[derive(Serialize, Default, Clone, Debug)]
struct TrailerSpec {
    /// Unlike `name` (only sent from ETS2 1.14/ATS 1.01 onward - blank on
    /// older game versions), `id` is available from every version this
    /// plugin supports, so it's the only reliable way to tell whether two
    /// `trailer` configuration events describe the same physical trailer.
    id: String,
    name: String,
    /// The kingpin coupling point, in the trailer's own local space - the
    /// trailer-side counterpart to `TruckSpec::hook_position`.
    hook_position: LocalPoint,
    wheel_liftable: Vec<bool>,
}

/// Everything read from the `job` configuration for whatever job is currently
/// active (freight or, once SCS publishes it, anything else that reuses this
/// same configuration id).
#[derive(Serialize, Default, Clone, Debug)]
struct JobInfo {
    active: bool,
    /// Raw configuration id this job came from - `"job"` for classic freight,
    /// `"car_job"` for a Road Trip DLC Quick Job, `"bus_job"` for ETS2's
    /// Coaches DLC. Kept as the literal SDK string rather than an enum since
    /// this build's typed catalog (audited against SDK 1.14, which predates
    /// all three of these being distinguished) has no constant for the last
    /// two - see `handle_configuration`'s doc comment for how this was found.
    source_kind: String,
    cargo: String,
    cargo_id: String,
    cargo_mass_kg: f32,
    source_city: String,
    source_city_id: String,
    source_company: String,
    source_company_id: String,
    destination_city: String,
    destination_city_id: String,
    destination_company: String,
    destination_company_id: String,
    income: u64,
    /// Despite the name (kept for wire compatibility), this is NOT a
    /// countdown in seconds - it's the SDK's `job.delivery.time` attribute,
    /// an absolute deadline on the in-game clock, in game-minutes (the same
    /// units/epoch as the `game_time_minutes` channel). Minutes remaining is
    /// `delivery_time_seconds - game_time_minutes`, not this field alone.
    delivery_time_seconds: u32,
    planned_distance_km: u32,
    cargo_loaded: bool,
    special_job: bool,
    /// Raw SDK string, kept alongside `job_market_known` on purpose - see
    /// `handle_configuration`'s doc comment for why this must never be
    /// collapsed down to only the typed enum.
    job_market_raw: String,
    job_market_known: Option<String>,
}

/// One physical wheel position's live state (suspension, contact, rotation).
/// Truck and trailer each keep their own `Vec<WheelState>`, grown on demand
/// as indexed channel updates arrive (see `ensure_wheel_len`) rather than
/// pre-sized to a fixed count - a car has fewer wheel positions than a truck
/// with a tag axle, and a short trailer has fewer than a tridem one, so a
/// fixed-size array would either truncate a real rig or pad a car's output
/// with meaningless zero entries.
#[derive(Serialize, Default, Clone, Copy, Debug)]
struct WheelState {
    suspension_deflection: f32,
    on_ground: bool,
    substance: u32,
    velocity: f32,
    steering: f32,
    rotation: f32,
    lift: f32,
    lift_offset: f32,
}

/// Upper bound for reading the indexed wheel attributes (`wheel.steerable`
/// and friends) out of a configuration event, which stops at the first
/// missing index anyway. The wheel *channel* subscriptions use the exact
/// per-kind limits below instead.
const MAX_WHEEL_INDEX: u32 = 24;

/// How many wheel indices the games actually accept for the `truck.wheel.*`
/// and `trailer.wheel.*` channels. Subscribing past these still works
/// (`subscribe_at_optional` tolerates it) but makes the SDK write an ERROR
/// line to the game log for every rejected index - 112 of them per game
/// start before these limits were applied. The values are what ATS 1.61.3.1
/// and ETS2 1.61.1.1 both reported: truck indices 14+ and trailer indices
/// 20+ are "not valid". They can't be sized from the real wheel count
/// instead, since the framework only allows subscribing during `initialize`,
/// before any configuration event has said how many wheels a vehicle has.
const MAX_TRUCK_WHEEL_INDEX: u32 = 14;
const MAX_TRAILER_WHEEL_INDEX: u32 = 20;

/// One H-shifter gear-slot mapping, decoded from the `hshifter` configuration.
#[derive(Serialize, Clone, Debug)]
struct HshifterSlot {
    slot: u32,
    gear: i32,
    handle_position: u32,
    selectors: u32,
}

#[derive(Serialize, Default, Clone, Debug)]
struct HshifterInfo {
    selector_count: u32,
    slots: Vec<HshifterSlot>,
}

#[derive(Serialize, Default, Clone, Debug)]
struct TrailerState {
    connected: bool,
    cargo_damage: f32,
    placement: Placement,
    wear_body: f32,
    wear_chassis: f32,
    wear_wheels: f32,
    wheels: Vec<WheelState>,
}

/// The live, frequently-changing part of the state - one field per subscribed
/// channel. Numeric fields default to zero/false rather than `Option` because
/// SCS channels report "no value" as an SDK-level absence handled by
/// `ChannelUpdate::value` already (see `handle_channel`) - once a value has
/// ever arrived it stays meaningful for the rest of the session, matching the
/// upstream example's own "leave the previous field unchanged" approach.
#[derive(Serialize, Default, Clone, Debug)]
struct Snapshot {
    game_paused: bool,
    placement: Placement,
    speed_mps: f32,
    engine_rpm: f32,
    engine_gear: i32,
    displayed_gear: i32,
    cruise_control_mps: f32,
    parking_brake: bool,
    motor_brake: bool,
    retarder_level: u32,
    brake_air_pressure: f32,
    brake_air_pressure_warning: bool,
    brake_air_pressure_emergency: bool,
    brake_temperature: f32,
    fuel_liters: f32,
    fuel_warning: bool,
    fuel_average_consumption: f32,
    fuel_range_km: f32,
    adblue_liters: f32,
    adblue_warning: bool,
    adblue_average_consumption: f32,
    oil_pressure: f32,
    oil_pressure_warning: bool,
    oil_temperature: f32,
    water_temperature: f32,
    water_temperature_warning: bool,
    battery_voltage: f32,
    battery_voltage_warning: bool,
    electric_enabled: bool,
    engine_enabled: bool,
    light_lblinker: bool,
    light_rblinker: bool,
    hazard_warning: bool,
    light_parking: bool,
    light_low_beam: bool,
    light_high_beam: bool,
    light_beacon: bool,
    light_brake: bool,
    light_reverse: bool,
    differential_lock: bool,
    lift_axle: bool,
    lift_axle_indicator: bool,
    trailer_lift_axle: bool,
    trailer_lift_axle_indicator: bool,
    wear_engine: f32,
    wear_transmission: f32,
    wear_cabin: f32,
    wear_chassis: f32,
    wear_wheels: f32,
    odometer_km: f32,
    navigation_distance_m: f32,
    navigation_time_s: f32,
    navigation_speed_limit_mps: f32,
    truck_wheels: Vec<WheelState>,
    /// Current physical H-shifter slot - what Ctrl+Alt+G-style calibration
    /// actually captures (distinct from the `hshifter` configuration's static
    /// slot->gear mapping, decoded separately into `HshifterInfo`).
    hshifter_slot: u32,
    /// Selector toggle states, indexed 0=splitter, 1=range (matching
    /// TruckHub's own already-calibrated convention) - grown on demand like
    /// the wheel vecs, since a gearbox with no splitter/range never delivers
    /// index 1 at all.
    hshifter_selectors: Vec<bool>,
    trailer: TrailerState,
    game_time_minutes: u32,
    next_rest_stop_minutes: i32,
    multiplayer_time_offset_minutes: i32,
}

/// Shared, cheaply-cloneable handles that outlive any single callback - the
/// HTTP thread(s) and the SDK callback thread both hold these.
#[derive(Clone)]
struct Shared {
    snapshot: Arc<Mutex<Snapshot>>,
    truck_spec: Arc<Mutex<TruckSpec>>,
    trailer_spec: Arc<Mutex<TrailerSpec>>,
    hshifter: Arc<Mutex<HshifterInfo>>,
    job: Arc<Mutex<JobInfo>>,
    /// One sender per currently-open `/stream` connection. `broadcast` drops
    /// any sender whose receiver has already hung up instead of erroring,
    /// since HTTP clients disconnect at arbitrary times.
    subscribers: Arc<Mutex<Vec<mpsc::Sender<String>>>>,
    game_display_name: Arc<Mutex<String>>,
    /// Count of HTTP connections currently being handled, each on its own
    /// thread living inside this game process (see spawn_http_server) - capped
    /// at MAX_HTTP_CONNECTIONS so a runaway/reconnecting client can't grow
    /// threads unboundedly inside the host process.
    active_connections: Arc<AtomicUsize>,
    /// The running HTTP server, kept here so `Plugin::shutdown` can stop it -
    /// `spawn_http_server`'s own thread only holds a clone. `None` before it
    /// starts and again once it has been stopped.
    server: Arc<Mutex<Option<Arc<tiny_http::Server>>>>,
}

/// Hard cap on concurrent HTTP connections (each backed by its own OS thread
/// inside the game process). Generous for the real use case - one TruckHub
/// client holding a single long-lived `/stream` connection, plus occasional
/// one-shot `/snapshot` requests - while still bounding worst-case thread
/// growth from a misbehaving client.
const MAX_HTTP_CONNECTIONS: usize = 32;

impl Shared {
    fn new() -> Self {
        Self {
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            truck_spec: Arc::new(Mutex::new(TruckSpec::default())),
            trailer_spec: Arc::new(Mutex::new(TrailerSpec::default())),
            hshifter: Arc::new(Mutex::new(HshifterInfo::default())),
            job: Arc::new(Mutex::new(JobInfo::default())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            game_display_name: Arc::new(Mutex::new(String::new())),
            active_connections: Arc::new(AtomicUsize::new(0)),
            server: Arc::new(Mutex::new(None)),
        }
    }

    /// Tells every open `/stream` client the plugin is going away, then ends
    /// their streams. Dropping the senders is what ends them: `serve_stream`'s
    /// loop sees the channel disconnect once it's empty, so each client still
    /// receives the `disconnected` message queued just before.
    fn end_streams(&self) {
        self.broadcast(&serde_json::json!({ "type": "disconnected" }).to_string());
        self.subscribers.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// Stops accepting connections and closes the listening socket.
    ///
    /// `unblock` ends the accept-loop thread's iterator; once that thread
    /// drops its own clone the `tiny_http::Server` is dropped, which wakes its
    /// internal accept thread and releases the port. Without this a client
    /// retrying after the game has unloaded the plugin would still be
    /// accepted by a listener whose code is gone.
    fn stop_http_server(&self) {
        let server = self.server.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(server) = server {
            server.unblock();
        }
    }

    /// Sends one already-serialized JSON line to every open `/stream`
    /// connection. Called from the SDK callback thread, so this must stay
    /// non-blocking: `mpsc::Sender::send` only pushes onto an unbounded queue,
    /// it never waits on the receiving HTTP thread.
    fn broadcast(&self, message: &str) {
        let mut subs = self.subscribers.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|tx| tx.send(message.to_string()).is_ok());
    }

    /// Applies a `trailer` configuration event's effect on trailer state.
    /// Split out of `handle_configuration` into something that takes an
    /// already-decoded `TrailerSpec` (or `None` for an empty event) instead
    /// of a `ConfigurationEvent` directly, since that type's constructor is
    /// private to the SDK framework crate and can't be built from a unit
    /// test here - same reasoning as calling `Plugin`'s own methods directly
    /// in the render-time-restart test instead of mocking `FrameStartRef`.
    ///
    /// `Some(spec)` (a non-empty event) replaces the stored spec outright.
    /// `None` (an empty event) deliberately does NOT touch the stored spec -
    /// confirmed live, an empty `trailer` config arrives both on a genuine
    /// detach and as a spurious resend on a menu open/reload while the same
    /// trailer is still physically hooked, so wiping id/name/hook_position
    /// here would lose real data for no reason. What an empty event DOES
    /// invalidate is the live `connected`/`placement` channel state: SCS
    /// leaves those frozen at their last reading with no further update once
    /// a trailer stops being described, rather than ever reporting them as
    /// invalid - live testing caught the hub reporting `connected=true` and
    /// a frozen placement for a trailer the truck had already driven 220 m
    /// away from, because nothing else ever told it the data had gone stale.
    fn apply_trailer_configuration(&self, spec: Option<TrailerSpec>) {
        match spec {
            Some(spec) => *self.trailer_spec.lock().unwrap_or_else(|e| e.into_inner()) = spec,
            None => {
                let mut snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
                snapshot.trailer.connected = false;
                snapshot.trailer.placement = Placement::default();
            }
        }
    }

    fn full_state_json(&self) -> serde_json::Value {
        let snapshot = self.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let truck_spec = self.truck_spec.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let trailer_spec = self.trailer_spec.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let hshifter = self.hshifter.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let job = self.job.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let game_display_name = self
            .game_display_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        serde_json::json!({
            "type": "snapshot",
            "game": game_display_name,
            "truck_spec": truck_spec,
            "trailer_spec": trailer_spec,
            "hshifter": hshifter,
            "job": job,
            "data": snapshot,
        })
    }
}

/// Decrements `active_connections` when dropped, so the count is released on
/// every exit path out of the request-handling thread - normal completion,
/// an early return, or a panic unwinding the thread - without needing a
/// matching decrement at each one individually.
struct ConnectionGuard(Arc<AtomicUsize>);
impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn handle_request(request: tiny_http::Request, shared: &Shared) {
    if *request.method() == tiny_http::Method::Get && request.url() == "/stream" {
        let (tx, rx) = mpsc::channel();
        // Replay the current full state as the stream's first event so a
        // client that just connected doesn't have to wait for the next
        // change to know where things stand.
        let _ = tx.send(shared.full_state_json().to_string());
        shared.subscribers.lock().unwrap_or_else(|e| e.into_inner()).push(tx);
        serve_stream(request, rx);
        return;
    }

    let response_result = match (request.method(), request.url()) {
        (tiny_http::Method::Get, "/snapshot") => {
            let body = shared.full_state_json().to_string();
            request.respond(
                tiny_http::Response::from_string(body).with_header(
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .expect("static header is always valid"),
                ),
            )
        }
        _ => request.respond(tiny_http::Response::from_string("not found").with_status_code(404)),
    };
    if let Err(error) = response_result {
        // A client disconnecting mid-response is routine, not a plugin bug -
        // logging it would just be noise on every ordinary tab close.
        let _ = error;
    }
}

/// Serves `/stream` by writing straight to the raw connection
/// (`Request::into_writer`), bypassing tiny_http's normal `Response`/
/// `Read`-body path entirely. That path only flushes once the body reader
/// reaches EOF (see `tiny_http`'s own `Response::raw_print` doc: "does not
/// flush the writer", and its client code wraps the socket in a 1 KB
/// `BufWriter`) - fine for a response built in one go, but for a long-lived
/// SSE connection it meant headers and small periodic messages all sat
/// unflushed in that buffer until it filled or the connection closed,
/// observed live as a client getting literally 0 bytes - not even the status
/// line - for minutes during a stall. Writing and flushing by hand after
/// every message fixes that, and the periodic keepalive doubles as a
/// liveness probe: a dead client is now noticed within one
/// `STREAM_KEEPALIVE_INTERVAL` (a failed write), instead of staying parked in
/// a blocking read indefinitely and leaking its connection slot until the
/// next real broadcast happened to expose the dead socket.
///
/// The request's own `Connection` header is deliberately not consulted -
/// there's nothing to end early for a stream that only stops when the plugin
/// shuts down or the client disconnects - and the hand-rolled chunk framing
/// below keeps the same `Transfer-Encoding: chunked` wire format the
/// previous `Response`-based implementation used, since a keep-alive client
/// (TruckHub's `HttpClient` among them) relies on the trailing zero-length
/// chunk, not a closed socket, to know the stream ended.
fn serve_stream(request: tiny_http::Request, rx: mpsc::Receiver<String>) {
    let mut writer = request.into_writer();
    let header = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n";
    if writer.write_all(header).is_err() || writer.flush().is_err() {
        return;
    }
    loop {
        let chunk = match rx.recv_timeout(STREAM_KEEPALIVE_INTERVAL) {
            Ok(message) => format!("data: {message}\n\n"),
            Err(mpsc::RecvTimeoutError::Timeout) => ": ping\n\n".to_string(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break, // plugin shut down
        };
        let size_line = format!("{:x}\r\n", chunk.len());
        let wrote = writer
            .write_all(size_line.as_bytes())
            .and_then(|_| writer.write_all(chunk.as_bytes()))
            .and_then(|_| writer.write_all(b"\r\n"))
            .and_then(|_| writer.flush());
        if wrote.is_err() {
            break; // client gone - the slot frees as soon as this thread returns
        }
    }
    let _ = writer.write_all(b"0\r\n\r\n");
    let _ = writer.flush();
}

/// Keeps this DLL mapped for the rest of the process's life, so nothing it
/// spawned can ever run code from an image the game has already unloaded.
///
/// The game calls `scs_telemetry_shutdown` and then unmaps the DLL, but
/// `tiny_http` owns detached worker threads this plugin has no handle to (its
/// accept thread, and a pool of workers that only exit ~5 seconds after the
/// server is dropped), so they can't be joined before that unload. A thread
/// that wakes inside unmapped code is an access violation at game exit; one
/// such fault was logged against `scs_telemetry_hub.dll_unloaded`. Pinning
/// makes the game's unload a no-op for this module instead - the threads then
/// wind down harmlessly and everything is released when the process ends.
#[cfg(windows)]
mod module_pin {
    use std::ffi::c_void;

    const GET_MODULE_HANDLE_EX_FLAG_PIN: u32 = 0x0000_0001;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x0000_0004;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetModuleHandleExW(flags: u32, module_name: *const u16, module: *mut *mut c_void) -> i32;
    }

    /// Returns whether the module containing this function is now pinned.
    pub fn pin_current_module() -> bool {
        let mut module: *mut c_void = std::ptr::null_mut();
        // FROM_ADDRESS makes the second argument an address inside the module
        // (not a name), and this function's own address is one. PIN needs no
        // matching release - the module simply stays loaded until process exit.
        let address = pin_current_module as *const ();
        // SAFETY: `address` points into this module and `module` is a valid
        // out-pointer for the duration of the call.
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
                address as *const u16,
                &mut module,
            ) != 0
        }
    }
}

#[cfg(not(windows))]
mod module_pin {
    /// This plugin only targets the Windows game builds.
    pub fn pin_current_module() -> bool {
        false
    }
}

/// Binds the HTTP server and starts serving it on its own thread. `tiny_http`'s
/// server loop is synchronous and would otherwise block accepting new
/// connections for as long as a `/stream` client stays open, so every
/// individual request is handed off to its own short-lived thread immediately
/// after being accepted.
///
/// Binding happens here, synchronously, so a failure (address already in use -
/// e.g. ATS and ETS2 both running) is reported to the caller instead of
/// silently leaving a plugin with no server. The server is also stored in
/// `shared.server` so `Shared::stop_http_server` can stop it later.
fn start_http_server(shared: &Shared, address: &str) -> Result<(), String> {
    let server = Arc::new(tiny_http::Server::http(address).map_err(|error| error.to_string())?);
    *shared.server.lock().unwrap_or_else(|e| e.into_inner()) = Some(server.clone());

    let shared = shared.clone();
    thread::spawn(move || {
        for request in server.incoming_requests() {
            // `/snapshot` completes on its own thread almost immediately and
            // never holds a slot open, unlike `/stream` - so it's exempted
            // from the cap entirely. Without this, a pile-up of long-lived
            // `/stream` connections (e.g. reconnecting clients during a
            // stall) could reach the cap and start 503-ing even a client that
            // just wants a one-shot read.
            if request.url() == "/snapshot" {
                let shared = shared.clone();
                thread::spawn(move || handle_request(request, &shared));
                continue;
            }

            // Reserve a slot before spawning - fetch_update only succeeds (and
            // increments) while under the cap, so a request arriving at the
            // cap is rejected here rather than after already spawning a
            // thread for it.
            let reserved = shared
                .active_connections
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n < MAX_HTTP_CONNECTIONS).then_some(n + 1)
                })
                .is_ok();
            if !reserved {
                let _ = request.respond(
                    tiny_http::Response::from_string("too many connections").with_status_code(503),
                );
                continue;
            }

            let shared = shared.clone();
            thread::spawn(move || {
                let _guard = ConnectionGuard(shared.active_connections.clone());
                handle_request(request, &shared);
            });
        }
    });
    Ok(())
}

/// Grows `wheels` with default entries so index `i` is valid, if it isn't
/// already - wheel-indexed updates can arrive in any order, and a later
/// index (e.g. a rear tag axle) may be updated before an earlier one.
fn ensure_wheel_len(wheels: &mut Vec<WheelState>, i: usize) {
    if wheels.len() <= i {
        wheels.resize(i + 1, WheelState::default());
    }
}

/// Decodes the `truck` static-specs configuration into [`TruckSpec`].
fn decode_truck_spec(event: &ConfigurationEvent<'_>) -> TruckSpec {
    TruckSpec {
        brand_id: event.string_owned(configuration::attributes::BRAND_ID).unwrap_or_default(),
        brand: event.string_owned(configuration::attributes::BRAND).unwrap_or_default(),
        id: event.string_owned(configuration::attributes::ID).unwrap_or_default(),
        name: event.string_owned(configuration::attributes::NAME).unwrap_or_default(),
        license_plate: event.string_owned(configuration::attributes::LICENSE_PLATE).unwrap_or_default(),
        license_plate_country: event.string_owned(configuration::attributes::LICENSE_PLATE_COUNTRY).unwrap_or_default(),
        fuel_capacity_liters: event.get(configuration::attributes::FUEL_CAPACITY).unwrap_or_default(),
        fuel_warning_factor: event.get(configuration::attributes::FUEL_WARNING_FACTOR).unwrap_or_default(),
        adblue_capacity_liters: event.get(configuration::attributes::ADBLUE_CAPACITY).unwrap_or_default(),
        adblue_warning_factor: event.get(configuration::attributes::ADBLUE_WARNING_FACTOR).unwrap_or_default(),
        rpm_limit: event.get(configuration::attributes::RPM_LIMIT).unwrap_or_default(),
        forward_gear_count: event.get(configuration::attributes::FORWARD_GEAR_COUNT).unwrap_or_default(),
        reverse_gear_count: event.get(configuration::attributes::REVERSE_GEAR_COUNT).unwrap_or_default(),
        retarder_step_count: event.get(configuration::attributes::RETARDER_STEP_COUNT).unwrap_or_default(),
        hook_position: event.get(configuration::attributes::HOOK_POSITION).map(LocalPoint::from).unwrap_or_default(),
        wheel_steerable: decode_indexed_bools(event, configuration::attributes::WHEEL_STEERABLE),
        wheel_powered: decode_indexed_bools(event, configuration::attributes::WHEEL_POWERED),
        wheel_liftable: decode_indexed_bools(event, configuration::attributes::WHEEL_LIFTABLE),
        // Overwritten by the caller right after this returns (see
        // handle_configuration's TRUCK branch) - this event has no
        // shifter_type attribute of its own, that comes from `controls`.
        shifter_type: String::new(),
    }
}

/// Decodes the `trailer` static-specs configuration into [`TrailerSpec`].
/// Shares the `name`/`WHEEL_LIFTABLE` attribute names with `truck` - SCS
/// reuses the same attribute name across multiple configuration ids, the
/// same pattern that let `decode_job` work unchanged against `car_job`.
fn decode_trailer_spec(event: &ConfigurationEvent<'_>) -> TrailerSpec {
    TrailerSpec {
        id: event.string_owned(configuration::attributes::ID).unwrap_or_default(),
        name: event.string_owned(configuration::attributes::NAME).unwrap_or_default(),
        hook_position: event.get(configuration::attributes::HOOK_POSITION).map(LocalPoint::from).unwrap_or_default(),
        wheel_liftable: decode_indexed_bools(event, configuration::attributes::WHEEL_LIFTABLE),
    }
}

/// Reads every index of an indexed boolean attribute up to `MAX_WHEEL_INDEX`,
/// stopping at the first index that returns nothing - real wheel indices are
/// contiguous starting at 0, so a gap means the real count has been reached.
fn decode_indexed_bools(event: &ConfigurationEvent<'_>, attribute: Attribute<bool>) -> Vec<bool> {
    let mut values = Vec::new();
    for i in 0..MAX_WHEEL_INDEX {
        let Some(index) = scs_sdk_plugin::sdk::SdkIndex::new(i) else { break };
        let Some(value) = event.get_at(attribute, index) else { break };
        values.push(value);
    }
    values
}

/// Upper bound on H-shifter gear slots this build looks for - real ATS/ETS2
/// trucks use well under this (typically under 20 counting range/splitter
/// combinations); generous rather than exact for the same reason as
/// `MAX_WHEEL_INDEX` above.
const MAX_HSHIFTER_SLOTS: u32 = 32;

/// Decodes the `hshifter` configuration into [`HshifterInfo`] - this is what
/// TruckHub's own H-shifter calibration feature (Ctrl+Alt+G gear capture)
/// needs: which physical slot/selector combination maps to which real gear.
fn decode_hshifter(event: &ConfigurationEvent<'_>) -> HshifterInfo {
    let selector_count = event.get(configuration::attributes::SELECTOR_COUNT).unwrap_or_default();
    let mut slots = Vec::new();
    for slot in 0..MAX_HSHIFTER_SLOTS {
        let Some(index) = scs_sdk_plugin::sdk::SdkIndex::new(slot) else { continue };
        let Some(gear) = event.get_at(configuration::attributes::SLOT_GEAR, index) else { continue };
        let handle_position = event.get_at(configuration::attributes::SLOT_HANDLE_POSITION, index).unwrap_or_default();
        let selectors = event.get_at(configuration::attributes::SLOT_SELECTORS, index).unwrap_or_default();
        slots.push(HshifterSlot { slot, gear, handle_position, selectors });
    }
    HshifterInfo { selector_count, slots }
}

/// Decodes the `job` configuration into [`JobInfo`].
///
/// `job_market_raw` and `job_market_known` are kept side by side deliberately:
/// this plugin was written specifically because a newer job type (ATS's Road
/// Trip DLC "Quick Jobs") wasn't visible through the previous telemetry
/// bridge at all. If SCS ever adds a `job.market` value this build's typed
/// enum doesn't recognize yet, `job_market_raw` still carries it through
/// untouched instead of silently dropping it - a consumer can act on the raw
/// string today and doesn't have to wait for a plugin rebuild.
///
/// `car_job` is not just `job` under a different id - confirmed by diffing
/// the real official SDK 1.15 headers (this crate audits 1.14): its market
/// attribute is named `car_job.market`, not `job.market`, and it has no
/// `cargo_mass`/`is_cargo_loaded`/`special_job` attributes at all (a car job
/// genuinely has none of those concepts). Found live: this plugin's very
/// first car_job support silently read the wrong attribute name and always
/// got back an empty market string - not "unrecognized", just wrong name.
fn decode_job(event: &ConfigurationEvent<'_>, source_kind: &str) -> JobInfo {
    if !event.has_attributes() {
        return JobInfo { active: false, source_kind: source_kind.to_string(), ..JobInfo::default() };
    }
    let job_market_raw = if source_kind == "car_job" {
        let car_job_market =
            Attribute::<StringValue>::new(c"car_job.market", GameSchemaAvailability::new(None, None));
        event.string_owned(car_job_market).unwrap_or_default()
    } else {
        event.string_owned(configuration::attributes::JOB_MARKET).unwrap_or_default()
    };
    JobInfo {
        active: true,
        source_kind: source_kind.to_string(),
        cargo: event.string_owned(configuration::attributes::CARGO).unwrap_or_default(),
        cargo_id: event.string_owned(configuration::attributes::CARGO_ID).unwrap_or_default(),
        cargo_mass_kg: event.get(configuration::attributes::CARGO_MASS).unwrap_or_default(),
        source_city: event.string_owned(configuration::attributes::SOURCE_CITY).unwrap_or_default(),
        source_city_id: event.string_owned(configuration::attributes::SOURCE_CITY_ID).unwrap_or_default(),
        source_company: event.string_owned(configuration::attributes::SOURCE_COMPANY).unwrap_or_default(),
        source_company_id: event.string_owned(configuration::attributes::SOURCE_COMPANY_ID).unwrap_or_default(),
        destination_city: event.string_owned(configuration::attributes::DESTINATION_CITY).unwrap_or_default(),
        destination_city_id: event.string_owned(configuration::attributes::DESTINATION_CITY_ID).unwrap_or_default(),
        destination_company: event.string_owned(configuration::attributes::DESTINATION_COMPANY).unwrap_or_default(),
        destination_company_id: event.string_owned(configuration::attributes::DESTINATION_COMPANY_ID).unwrap_or_default(),
        income: event.get(configuration::attributes::INCOME).unwrap_or_default(),
        delivery_time_seconds: event.get(configuration::attributes::DELIVERY_TIME).unwrap_or_default(),
        planned_distance_km: event.get(configuration::attributes::PLANNED_DISTANCE_KM).unwrap_or_default(),
        cargo_loaded: event.get(configuration::attributes::IS_CARGO_LOADED).unwrap_or_default(),
        special_job: event.get(configuration::attributes::SPECIAL_JOB).unwrap_or_default(),
        job_market_raw,
        job_market_known: event.job_market().map(|market| format!("{market:?}")),
    }
}

#[derive(Default)]
struct Plugin {
    shared: Shared,
    http_server_started: AtomicBool,
    current_render_time: AtomicU64,
    last_snapshot_broadcast_time: AtomicU64,
    /// Refuel-detection state - see `detect_refuel`'s doc comment. Plain fields
    /// (not Arc/Mutex/Atomic) because `detect_refuel` runs from `event`, which
    /// the trait already gives `&mut self` for - no interior mutability needed.
    refuel_last_fuel: f32,
    refuel_ticker: u32,
    refuel_stall_windows: u32,
    /// Mirrors RenCloud's *internal* `refuel` cycle flag - true from the first
    /// fuel-increase tick until the engine restarts, independent of
    /// `client_refuel_active` below (see the doc comment on why these two are
    /// deliberately different signals, not a duplicate).
    refuel_cycle_pending: bool,
    /// Mirrors RenCloud's *client-visible* `special_b.refuel` - can flip back
    /// to false (firing a "refuel_ended" event) on any fuel decrease, even
    /// while `refuel_cycle_pending` stays true awaiting an engine restart to
    /// fire "refuel_paid". This is what TruckHub's existing RefuelStart/
    /// RefuelEnd log lines are edge-triggered from; "refuel_paid" is edge-
    /// triggered from `refuel_cycle_pending` instead.
    client_refuel_active: bool,
    refuel_tmp_fuel: f32,
    refuel_start_fuel: f32,
}

impl Default for Shared {
    fn default() -> Self {
        Shared::new()
    }
}

impl Plugin {
    /// Applies one channel callback to the live snapshot. Only one branch
    /// ever matches per call (`ChannelUpdate::value` checks both the
    /// descriptor and the SDK's own type tag), so an if-else chain - not a
    /// match - is the natural shape here, same as the upstream example.
    fn handle_channel(&self, update: ChannelUpdate<'_>) {
        let mut snapshot = self.shared.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(v) = update.value(channels::truck::WORLD_PLACEMENT) {
            snapshot.placement = Placement {
                position: Vec3 { x: v.position.x, y: v.position.y, z: v.position.z },
                orientation: Orientation { heading: v.orientation.heading, pitch: v.orientation.pitch, roll: v.orientation.roll },
            };
        } else if let Some(v) = update.value(channels::truck::SPEED) { snapshot.speed_mps = v; }
        else if let Some(v) = update.value(channels::truck::ENGINE_RPM) { snapshot.engine_rpm = v; }
        else if let Some(v) = update.value(channels::truck::ENGINE_GEAR) { snapshot.engine_gear = v; }
        else if let Some(v) = update.value(channels::truck::DISPLAYED_GEAR) { snapshot.displayed_gear = v; }
        else if let Some(v) = update.value(channels::truck::CRUISE_CONTROL) { snapshot.cruise_control_mps = v; }
        else if let Some(v) = update.value(channels::truck::PARKING_BRAKE) { snapshot.parking_brake = v; }
        else if let Some(v) = update.value(channels::truck::MOTOR_BRAKE) { snapshot.motor_brake = v; }
        else if let Some(v) = update.value(channels::truck::RETARDER_LEVEL) { snapshot.retarder_level = v; }
        else if let Some(v) = update.value(channels::truck::BRAKE_AIR_PRESSURE) { snapshot.brake_air_pressure = v; }
        else if let Some(v) = update.value(channels::truck::BRAKE_AIR_PRESSURE_WARNING) { snapshot.brake_air_pressure_warning = v; }
        else if let Some(v) = update.value(channels::truck::BRAKE_AIR_PRESSURE_EMERGENCY) { snapshot.brake_air_pressure_emergency = v; }
        else if let Some(v) = update.value(channels::truck::BRAKE_TEMPERATURE) { snapshot.brake_temperature = v; }
        else if let Some(v) = update.value(channels::truck::FUEL) { snapshot.fuel_liters = v; }
        else if let Some(v) = update.value(channels::truck::FUEL_WARNING) { snapshot.fuel_warning = v; }
        else if let Some(v) = update.value(channels::truck::FUEL_AVERAGE_CONSUMPTION) { snapshot.fuel_average_consumption = v; }
        else if let Some(v) = update.value(channels::truck::FUEL_RANGE) { snapshot.fuel_range_km = v; }
        else if let Some(v) = update.value(channels::truck::ADBLUE) { snapshot.adblue_liters = v; }
        else if let Some(v) = update.value(channels::truck::ADBLUE_WARNING) { snapshot.adblue_warning = v; }
        else if let Some(v) = update.value(channels::truck::ADBLUE_AVERAGE_CONSUMPTION) { snapshot.adblue_average_consumption = v; }
        else if let Some(v) = update.value(channels::truck::OIL_PRESSURE) { snapshot.oil_pressure = v; }
        else if let Some(v) = update.value(channels::truck::OIL_PRESSURE_WARNING) { snapshot.oil_pressure_warning = v; }
        else if let Some(v) = update.value(channels::truck::OIL_TEMPERATURE) { snapshot.oil_temperature = v; }
        else if let Some(v) = update.value(channels::truck::WATER_TEMPERATURE) { snapshot.water_temperature = v; }
        else if let Some(v) = update.value(channels::truck::WATER_TEMPERATURE_WARNING) { snapshot.water_temperature_warning = v; }
        else if let Some(v) = update.value(channels::truck::BATTERY_VOLTAGE) { snapshot.battery_voltage = v; }
        else if let Some(v) = update.value(channels::truck::BATTERY_VOLTAGE_WARNING) { snapshot.battery_voltage_warning = v; }
        else if let Some(v) = update.value(channels::truck::ELECTRIC_ENABLED) { snapshot.electric_enabled = v; }
        else if let Some(v) = update.value(channels::truck::ENGINE_ENABLED) { snapshot.engine_enabled = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_LBLINKER) { snapshot.light_lblinker = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_RBLINKER) { snapshot.light_rblinker = v; }
        else if let Some(v) = update.value(channels::truck::HAZARD_WARNING) { snapshot.hazard_warning = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_PARKING) { snapshot.light_parking = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_LOW_BEAM) { snapshot.light_low_beam = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_HIGH_BEAM) { snapshot.light_high_beam = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_BEACON) { snapshot.light_beacon = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_BRAKE) { snapshot.light_brake = v; }
        else if let Some(v) = update.value(channels::truck::LIGHT_REVERSE) { snapshot.light_reverse = v; }
        else if let Some(v) = update.value(channels::truck::DIFFERENTIAL_LOCK) { snapshot.differential_lock = v; }
        else if let Some(v) = update.value(channels::truck::LIFT_AXLE) { snapshot.lift_axle = v; }
        else if let Some(v) = update.value(channels::truck::LIFT_AXLE_INDICATOR) { snapshot.lift_axle_indicator = v; }
        else if let Some(v) = update.value(channels::truck::TRAILER_LIFT_AXLE) { snapshot.trailer_lift_axle = v; }
        else if let Some(v) = update.value(channels::truck::TRAILER_LIFT_AXLE_INDICATOR) { snapshot.trailer_lift_axle_indicator = v; }
        else if let Some(v) = update.value(channels::truck::WEAR_ENGINE) { snapshot.wear_engine = v; }
        else if let Some(v) = update.value(channels::truck::WEAR_TRANSMISSION) { snapshot.wear_transmission = v; }
        else if let Some(v) = update.value(channels::truck::WEAR_CABIN) { snapshot.wear_cabin = v; }
        else if let Some(v) = update.value(channels::truck::WEAR_CHASSIS) { snapshot.wear_chassis = v; }
        else if let Some(v) = update.value(channels::truck::WEAR_WHEELS) { snapshot.wear_wheels = v; }
        else if let Some(v) = update.value(channels::truck::ODOMETER) { snapshot.odometer_km = v; }
        else if let Some(v) = update.value(channels::truck::NAVIGATION_DISTANCE) { snapshot.navigation_distance_m = v; }
        else if let Some(v) = update.value(channels::truck::NAVIGATION_TIME) { snapshot.navigation_time_s = v; }
        else if let Some(v) = update.value(channels::truck::NAVIGATION_SPEED_LIMIT) { snapshot.navigation_speed_limit_mps = v; }
        else if let Some(v) = update.value(channels::trailer::CONNECTED) { snapshot.trailer.connected = v; }
        else if let Some(v) = update.value(channels::trailer::CARGO_DAMAGE) { snapshot.trailer.cargo_damage = v; }
        else if let Some(v) = update.value(channels::trailer::WORLD_PLACEMENT) {
            snapshot.trailer.placement = Placement {
                position: Vec3 { x: v.position.x, y: v.position.y, z: v.position.z },
                orientation: Orientation { heading: v.orientation.heading, pitch: v.orientation.pitch, roll: v.orientation.roll },
            };
        }
        else if let Some(v) = update.value(channels::trailer::WEAR_BODY) { snapshot.trailer.wear_body = v; }
        else if let Some(v) = update.value(channels::trailer::WEAR_CHASSIS) { snapshot.trailer.wear_chassis = v; }
        else if let Some(v) = update.value(channels::trailer::WEAR_WHEELS) { snapshot.trailer.wear_wheels = v; }
        else if let Some(v) = update.value(channels::common::GAME_TIME) { snapshot.game_time_minutes = v; }
        else if let Some(v) = update.value(channels::common::NEXT_REST_STOP) { snapshot.next_rest_stop_minutes = v; }
        else if let Some(v) = update.value(channels::common::MULTIPLAYER_TIME_OFFSET) { snapshot.multiplayer_time_offset_minutes = v; }
        else if let Some(v) = update.value(channels::truck::HSHIFTER_SLOT) { snapshot.hshifter_slot = v; }
        else if let Some(index) = update.index() {
            let i = index.raw() as usize;
            if let Some(v) = update.value(channels::truck::HSHIFTER_SELECTOR) {
                if snapshot.hshifter_selectors.len() <= i {
                    snapshot.hshifter_selectors.resize(i + 1, false);
                }
                snapshot.hshifter_selectors[i] = v;
            }
            else if let Some(v) = update.value(channels::truck::WHEEL_SUSP_DEFLECTION) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].suspension_deflection = v; }
            else if let Some(v) = update.value(channels::truck::WHEEL_ON_GROUND) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].on_ground = v; }
            else if let Some(v) = update.value(channels::truck::WHEEL_SUBSTANCE) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].substance = v; }
            else if let Some(v) = update.value(channels::truck::WHEEL_VELOCITY) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].velocity = v; }
            else if let Some(v) = update.value(channels::truck::WHEEL_STEERING) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].steering = v; }
            else if let Some(v) = update.value(channels::truck::WHEEL_ROTATION) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].rotation = v; }
            else if let Some(v) = update.value(channels::truck::WHEEL_LIFT) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].lift = v; }
            else if let Some(v) = update.value(channels::truck::WHEEL_LIFT_OFFSET) { ensure_wheel_len(&mut snapshot.truck_wheels, i); snapshot.truck_wheels[i].lift_offset = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_SUSP_DEFLECTION) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].suspension_deflection = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_ON_GROUND) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].on_ground = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_SUBSTANCE) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].substance = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_VELOCITY) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].velocity = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_STEERING) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].steering = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_ROTATION) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].rotation = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_LIFT) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].lift = v; }
            else if let Some(v) = update.value(channels::trailer::WHEEL_LIFT_OFFSET) { ensure_wheel_len(&mut snapshot.trailer.wheels, i); snapshot.trailer.wheels[i].lift_offset = v; }
        }
    }

    fn maybe_broadcast_snapshot(&self) {
        let now = self.current_render_time.load(Ordering::Relaxed);
        let last = self.last_snapshot_broadcast_time.load(Ordering::Relaxed);
        // A save/vehicle/profile reload resets the game's render-time clock to
        // a small value, so `now` can land far below `last` (the high-water
        // mark from before the reload). Plain `saturating_sub` turned that
        // into a permanent stall: `now - last` floors at 0 forever, which is
        // always "too soon", until the new timeline climbs back past the old
        // one - observed live as `/stream` going silent for minutes while
        // `/snapshot` stayed healthy. Treat a backwards jump as "broadcast
        // now" instead of computing a nonsensical elapsed time from it.
        if now >= last && now - last < SNAPSHOT_INTERVAL_MICROSECONDS {
            return;
        }
        self.last_snapshot_broadcast_time.store(now, Ordering::Relaxed);
        self.shared.broadcast(&self.shared.full_state_json().to_string());
    }

    /// Detects refueling and emits `refuel_started`/`refuel_ended`/`refuel_paid`
    /// events - the official SDK 1.14 gameplay-event catalog has none of
    /// these at all, confirmed by reading its real headers. Ported from
    /// RenCloud/scs-sdk-plugin's `scs-telemetry/src/scs_telemetry.cpp`
    /// (`telemetry_frame_start`, the exact per-tick fuel-delta heuristic
    /// TruckHub's existing RefuelStart/RefuelEnd/RefuelPayed log lines already
    /// come from) - found via a background research agent after confirming
    /// no official signal exists, since TruckHub needs the same behavior
    /// this new plugin is meant to replace.
    ///
    /// Two states, matching RenCloud's own two separate flags exactly (this
    /// isn't a simplification - both are load-bearing):
    /// - `client_refuel_active` = RenCloud's client-visible `special_b.refuel`:
    ///   true while fuel is trending up, false the instant it trends down -
    ///   edge-triggered here into `refuel_started`/`refuel_ended` events.
    /// - `refuel_cycle_pending` = RenCloud's internal `refuel`: stays true
    ///   from the first increase until the engine is next enabled, regardless
    ///   of what `client_refuel_active` does meanwhile - edge-triggered into
    ///   `refuel_paid`, matching the real behavior (paid fires on restarting
    ///   the engine after refueling, not on the fuel gauge itself settling).
    ///
    /// Deliberately NOT ported: RenCloud exposes `refuel`/`refuelPayed` as
    /// persistent booleans for a C# client to edge-detect itself, and its own
    /// changelog documents a real bug from this (`refuelPayed` never resets,
    /// so a later truck/profile change can misfire it - changelog.md #55 and
    /// the "known issues" entry above it). Firing one-shot events directly
    /// from the transition, right here, sidesteps that whole bug class - the
    /// pending flag always gets cleared at the exact instant its event fires.
    fn detect_refuel(&mut self) {
        let (current_fuel, engine_enabled) = {
            let snapshot = self.shared.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            (snapshot.fuel_liters, snapshot.engine_enabled)
        };
        let last_fuel = self.refuel_last_fuel;

        if !self.refuel_cycle_pending {
            self.refuel_start_fuel = self.refuel_tmp_fuel;
            self.refuel_tmp_fuel = current_fuel;
        }

        if current_fuel > last_fuel && last_fuel > 0.0 {
            self.refuel_stall_windows = 0;
            if !self.client_refuel_active {
                self.client_refuel_active = true;
                self.shared.broadcast(&serde_json::json!({"type":"event","data":{"name":"refuel_started"}}).to_string());
            }
            if !self.refuel_cycle_pending {
                self.refuel_cycle_pending = true;
            }
        } else if current_fuel < last_fuel && self.client_refuel_active {
            self.refuel_stall_windows = 0;
            self.client_refuel_active = false;
            self.shared.broadcast(&serde_json::json!({"type":"event","data":{"name":"refuel_ended"}}).to_string());
        }

        if self.refuel_cycle_pending && engine_enabled {
            self.refuel_cycle_pending = false;
            let amount = current_fuel - self.refuel_start_fuel;
            self.shared.broadcast(&serde_json::json!({"type":"event","data":{"name":"refuel_paid","amount_liters":amount}}).to_string());
        }

        // Plateau detector: a real refuel animation eventually just stops
        // (tank full, or player walks away) with no further decrease to
        // trigger the branch above - without this, `client_refuel_active`
        // would otherwise stay stuck true and never fire "ended". Checked
        // once every 10 ticks (matching RenCloud's own tick-count window, not
        // a fixed real-time duration - simulation tick rate isn't 1:1 with
        // wall-clock seconds); 5 stable windows in a row (~50-60 ticks) means
        // the value has been flat long enough to call it over.
        self.refuel_ticker += 1;
        if self.refuel_ticker > 10 {
            self.refuel_ticker = 0;
            if current_fuel == last_fuel {
                self.refuel_stall_windows += 1;
            } else {
                self.refuel_stall_windows = 0;
            }
            if self.refuel_stall_windows >= 5 {
                self.refuel_stall_windows = 0;
                if self.client_refuel_active {
                    self.client_refuel_active = false;
                    self.shared.broadcast(&serde_json::json!({"type":"event","data":{"name":"refuel_ended"}}).to_string());
                }
            }
        }

        self.refuel_last_fuel = current_fuel;
    }

    fn handle_configuration(&self, context: &PluginContext<'_>, event: ConfigurationEvent<'_>) {
        // Logged unconditionally, for every configuration id the game sends -
        // not just the ones this plugin has a typed decoder for. Written
        // specifically to check whether some Road Trip Quick Job might be
        // arriving under a configuration id this build's SDK catalog (audited
        // against SDK 1.14) doesn't have a named constant for; `.id()` reads
        // the raw string regardless of whether any typed `ConfigurationId`
        // constant recognizes it.
        context.message(format_args!(
            "[scs-telemetry-hub] configuration event: id={:?} has_attributes={}",
            event.id(),
            event.has_attributes()
        ));
        if event.is(configuration::ids::TRUCK) {
            let mut spec = decode_truck_spec(&event);
            // `truck` and `controls` are two separate configuration events with
            // no guaranteed ordering - preserve whichever shifter_type is
            // already known (from a `controls` event that arrived first)
            // instead of always resetting it to blank here.
            spec.shifter_type = self.shared.truck_spec.lock().unwrap_or_else(|e| e.into_inner()).shifter_type.clone();
            *self.shared.truck_spec.lock().unwrap_or_else(|e| e.into_inner()) = spec;
            return;
        }
        if event.is(configuration::ids::HSHIFTER) {
            let hshifter = decode_hshifter(&event);
            *self.shared.hshifter.lock().unwrap_or_else(|e| e.into_inner()) = hshifter;
            return;
        }
        if event.is(configuration::ids::CONTROLS) {
            let shifter_type = event.string_owned(configuration::attributes::SHIFTER_TYPE).unwrap_or_default();
            self.shared.truck_spec.lock().unwrap_or_else(|e| e.into_inner()).shifter_type = shifter_type;
            return;
        }
        // First trailer only, matching TruckHub's own existing convention
        // (see TelemetrySnapshot's doc comment) - a numbered "trailer.0" and
        // the legacy unnumbered "trailer" id both mean the same first slot,
        // just under different SDK schema generations.
        if event.is_legacy_trailer() || event.trailer_index() == Some(scs_sdk_plugin::sdk::TrailerIndex::ZERO) {
            let spec = event.has_attributes().then(|| decode_trailer_spec(&event));
            self.shared.apply_trailer_configuration(spec);
            return;
        }
        // `configuration::ids::JOB` only ever matches the literal "job" id.
        // "car_job" (Road Trip DLC Quick Jobs) and "bus_job" (ETS2 Coaches
        // DLC) are separate configuration ids this build's SDK catalog has no
        // typed constant for at all - confirmed live by the unconditional
        // `.id()` log above. `NamedValues::get`/`find` look attributes up by
        // raw name regardless of which configuration id they arrived under,
        // so the existing `decode_job` works unchanged against any of the
        // three - only the id has to be matched by raw string here instead of
        // the typed `ConfigurationId` constant.
        let raw_id = event.id();
        if raw_id == "job" || raw_id == "car_job" || raw_id == "bus_job" {
            // Confirmed live (2026-10-01): ATS sends all three of job/
            // car_job/bus_job together in one batch whenever job state
            // changes - only whichever one matches the job's actual type
            // carries real attributes, the other two arrive empty in the
            // same batch. Naively applying every event in arrival order
            // clobbered a genuine active job with a trailing empty one
            // (car_job arrives last, so it always won). Fix: an empty event
            // only clears our state if it's for the SAME id we currently
            // consider active - an empty event for a *different* id is just
            // noise from the other two job types and must be ignored.
            let has_attributes = event.has_attributes();
            let mut job_lock = self.shared.job.lock().unwrap_or_else(|e| e.into_inner());
            if has_attributes || job_lock.source_kind == raw_id {
                let job = decode_job(&event, &raw_id);
                *job_lock = job.clone();
                drop(job_lock);
                // Job changes matter immediately, not just on the next
                // throttled snapshot tick - a consumer waiting on this
                // shouldn't have to poll or wait up to
                // SNAPSHOT_INTERVAL_MICROSECONDS to see it.
                self.shared.broadcast(&serde_json::json!({ "type": "job", "data": job }).to_string());
            }
        }
    }

    fn handle_gameplay(&self, context: &PluginContext<'_>, event: GameplayEvent<'_>) {
        // Same reasoning as handle_configuration's unconditional log: catches
        // any gameplay event id this build's typed catalog doesn't recognize,
        // instead of silently dropping it in the final `else` branch below.
        context.message(format_args!("[scs-telemetry-hub] gameplay event: id={:?}", event.id()));
        let payload = if event.is(gameplay::events::JOB_DELIVERED) {
            serde_json::json!({
                "name": "job_delivered",
                "revenue": event.get(gameplay::attributes::REVENUE).unwrap_or_default(),
                "earned_xp": event.get(gameplay::attributes::EARNED_XP).unwrap_or_default(),
                "cargo_damage": event.get(gameplay::attributes::CARGO_DAMAGE).unwrap_or_default(),
                "distance_km": event.get(gameplay::attributes::DISTANCE_KM).unwrap_or_default(),
                "delivery_time": event.get(gameplay::attributes::DELIVERY_TIME).unwrap_or_default(),
                "auto_park_used": event.get(gameplay::attributes::AUTO_PARK_USED).unwrap_or_default(),
                "auto_load_used": event.get(gameplay::attributes::AUTO_LOAD_USED).unwrap_or_default(),
            })
        } else if event.is(gameplay::events::JOB_CANCELLED) {
            serde_json::json!({
                "name": "job_cancelled",
                "cancel_penalty": event.get(gameplay::attributes::CANCEL_PENALTY).unwrap_or_default(),
            })
        } else if event.is(gameplay::events::PLAYER_FINED) {
            serde_json::json!({
                "name": "player_fined",
                "offence_raw": event.string_owned(gameplay::attributes::FINE_OFFENCE).unwrap_or_default(),
                "offence_known": event.fine_offence().map(|o| format!("{o:?}")),
                "amount": event.get(gameplay::attributes::FINE_AMOUNT).unwrap_or_default(),
            })
        } else if event.is(gameplay::events::PLAYER_TOLLGATE_PAID) {
            serde_json::json!({
                "name": "tollgate_paid",
                "amount": event.get(gameplay::attributes::PAY_AMOUNT).unwrap_or_default(),
            })
        } else if event.is(gameplay::events::PLAYER_USE_FERRY) {
            serde_json::json!({
                "name": "ferry_used",
                "amount": event.get(gameplay::attributes::PAY_AMOUNT).unwrap_or_default(),
                "source_name": event.string_owned(gameplay::attributes::SOURCE_NAME).unwrap_or_default(),
                "target_name": event.string_owned(gameplay::attributes::TARGET_NAME).unwrap_or_default(),
            })
        } else if event.is(gameplay::events::PLAYER_USE_TRAIN) {
            serde_json::json!({
                "name": "train_used",
                "amount": event.get(gameplay::attributes::PAY_AMOUNT).unwrap_or_default(),
                "source_name": event.string_owned(gameplay::attributes::SOURCE_NAME).unwrap_or_default(),
                "target_name": event.string_owned(gameplay::attributes::TARGET_NAME).unwrap_or_default(),
            })
        } else if event.id() == "car_job.delivered" {
            // Not in this build's typed catalog at all (audited against SDK
            // 1.14; `car_job.delivered` is new in the real 1.15 headers) -
            // matched by raw id, same reasoning as handle_configuration's
            // car_job/bus_job fallback. Real 1.15 headers document a
            // `vehicle_damage` attribute here instead of `cargo_damage` (a
            // car job usually has no cargo to damage, but the vehicle
            // itself can be) - no typed constant exists for it yet, so it's
            // read via a hand-built descriptor rather than
            // `gameplay::attributes::*`.
            let vehicle_damage = Attribute::<f32>::new(c"vehicle_damage", GameSchemaAvailability::new(None, None));
            serde_json::json!({
                "name": "job_delivered",
                "revenue": event.get(gameplay::attributes::REVENUE).unwrap_or_default(),
                "earned_xp": event.get(gameplay::attributes::EARNED_XP).unwrap_or_default(),
                "vehicle_damage": event.get(vehicle_damage).unwrap_or_default(),
                "distance_km": event.get(gameplay::attributes::DISTANCE_KM).unwrap_or_default(),
                "delivery_time": event.get(gameplay::attributes::DELIVERY_TIME).unwrap_or_default(),
                "auto_park_used": event.get(gameplay::attributes::AUTO_PARK_USED).unwrap_or_default(),
                "auto_load_used": event.get(gameplay::attributes::AUTO_LOAD_USED).unwrap_or_default(),
            })
        } else if event.id() == "car_job.cancelled" {
            serde_json::json!({
                "name": "job_cancelled",
                "cancel_penalty": event.get(gameplay::attributes::CANCEL_PENALTY).unwrap_or_default(),
            })
        } else {
            return;
        };
        self.shared.broadcast(&serde_json::json!({ "type": "event", "data": payload }).to_string());
    }
}

impl TelemetryPlugin for Plugin {
    fn metadata(&self) -> PluginMetadata {
        PluginMetadata::new("SCS Telemetry Hub", env!("CARGO_PKG_VERSION"))
    }

    fn compatibility(&self) -> PluginCompatibility {
        PluginCompatibility::new(TelemetryApiVersion::V1_01, &SUPPORTED_GAMES)
    }

    fn initialize(&mut self, context: &mut PluginContext<'_>) -> PluginResult {
        let game_token = match context.game().kind() {
            Game::AmericanTruckSimulator => "ats",
            Game::EuroTruckSimulator2 => "ets2",
            Game::Other => "unknown",
        };
        *self.shared.game_display_name.lock().unwrap_or_else(|e| e.into_inner()) = game_token.to_string();

        context.subscribe_event(TelemetryEventKind::FrameStart)?;
        context.subscribe_event(TelemetryEventKind::FrameEnd)?;
        context.subscribe_event(TelemetryEventKind::Paused)?;
        context.subscribe_event(TelemetryEventKind::Started)?;
        context.subscribe_event(TelemetryEventKind::Configuration)?;
        context.subscribe_event(TelemetryEventKind::Gameplay)?;

        context.subscribe(channels::truck::WORLD_PLACEMENT)?;
        context.subscribe(channels::truck::SPEED)?;
        context.subscribe(channels::truck::ENGINE_RPM)?;
        context.subscribe(channels::truck::ENGINE_GEAR)?;
        context.subscribe(channels::truck::DISPLAYED_GEAR)?;
        context.subscribe(channels::truck::CRUISE_CONTROL)?;
        context.subscribe(channels::truck::PARKING_BRAKE)?;
        context.subscribe(channels::truck::MOTOR_BRAKE)?;
        context.subscribe(channels::truck::RETARDER_LEVEL)?;
        context.subscribe(channels::truck::BRAKE_AIR_PRESSURE)?;
        context.subscribe(channels::truck::BRAKE_AIR_PRESSURE_WARNING)?;
        context.subscribe(channels::truck::BRAKE_AIR_PRESSURE_EMERGENCY)?;
        context.subscribe(channels::truck::BRAKE_TEMPERATURE)?;
        context.subscribe(channels::truck::FUEL)?;
        context.subscribe(channels::truck::FUEL_WARNING)?;
        context.subscribe(channels::truck::FUEL_AVERAGE_CONSUMPTION)?;
        context.subscribe(channels::truck::FUEL_RANGE)?;
        // AdBlue is ETS2-only at the SDK level (ATS trucks don't expose it at
        // all, not even as a zeroed channel) - subscribing unconditionally
        // made ATS reject the whole plugin at startup, confirmed live via
        // ATS's own game.log.txt (successful API/schema negotiation followed
        // immediately by an unexplained shutdown, with no further logging -
        // traced to this exact subscription failing and propagating out of
        // `initialize` via `?`, since ADBLUE's declared availability is
        // `GameSchemaAvailability::new(Some(ets2::V1_12), None)` - `None` for
        // ATS, not just a higher version threshold).
        //
        // Even on ETS2 these are `subscribe_optional`, not `subscribe` -
        // confirmed live (2026-09-30) that a specific truck model without an
        // AdBlue system fitted doesn't register `truck.adblue.consumption.average`
        // at all (`SCS_RESULT_NOT_FOUND`), which crashed the whole plugin
        // init the exact same way the ATS-wide case did. Being ETS2 isn't
        // enough to guarantee a specific vehicle has these channels.
        if context.game().kind() == Game::EuroTruckSimulator2 {
            context.subscribe_optional(channels::truck::ADBLUE)?;
            context.subscribe_optional(channels::truck::ADBLUE_WARNING)?;
            context.subscribe_optional(channels::truck::ADBLUE_AVERAGE_CONSUMPTION)?;
        }
        context.subscribe(channels::truck::OIL_PRESSURE)?;
        context.subscribe(channels::truck::OIL_PRESSURE_WARNING)?;
        context.subscribe(channels::truck::OIL_TEMPERATURE)?;
        context.subscribe(channels::truck::WATER_TEMPERATURE)?;
        context.subscribe(channels::truck::WATER_TEMPERATURE_WARNING)?;
        context.subscribe(channels::truck::BATTERY_VOLTAGE)?;
        context.subscribe(channels::truck::BATTERY_VOLTAGE_WARNING)?;
        context.subscribe(channels::truck::ELECTRIC_ENABLED)?;
        context.subscribe(channels::truck::ENGINE_ENABLED)?;
        context.subscribe(channels::truck::LIGHT_LBLINKER)?;
        context.subscribe(channels::truck::LIGHT_RBLINKER)?;
        context.subscribe(channels::truck::HAZARD_WARNING)?;
        context.subscribe(channels::truck::LIGHT_PARKING)?;
        context.subscribe(channels::truck::LIGHT_LOW_BEAM)?;
        context.subscribe(channels::truck::LIGHT_HIGH_BEAM)?;
        context.subscribe(channels::truck::LIGHT_BEACON)?;
        context.subscribe(channels::truck::LIGHT_BRAKE)?;
        context.subscribe(channels::truck::LIGHT_REVERSE)?;
        context.subscribe(channels::truck::DIFFERENTIAL_LOCK)?;
        context.subscribe(channels::truck::LIFT_AXLE)?;
        context.subscribe(channels::truck::LIFT_AXLE_INDICATOR)?;
        context.subscribe(channels::truck::TRAILER_LIFT_AXLE)?;
        context.subscribe(channels::truck::TRAILER_LIFT_AXLE_INDICATOR)?;
        context.subscribe(channels::truck::WEAR_ENGINE)?;
        context.subscribe(channels::truck::WEAR_TRANSMISSION)?;
        context.subscribe(channels::truck::WEAR_CABIN)?;
        context.subscribe(channels::truck::WEAR_CHASSIS)?;
        context.subscribe(channels::truck::WEAR_WHEELS)?;
        context.subscribe(channels::truck::ODOMETER)?;
        context.subscribe(channels::truck::NAVIGATION_DISTANCE)?;
        context.subscribe(channels::truck::NAVIGATION_TIME)?;
        context.subscribe(channels::truck::NAVIGATION_SPEED_LIMIT)?;
        // EACH_FRAME, not the default on-change delivery: SCS only re-sends a
        // channel when its value changes, so `connected` would otherwise
        // never heal itself if anything else (an empty `trailer`
        // configuration event, see `Shared::apply_trailer_configuration`)
        // ever forces it to a wrong value - there would be no next "change"
        // to correct it. Re-asserting the true value every frame means a
        // wrong forced reset lasts at most one frame while the trailer is
        // actually still attached.
        context.subscribe_with_flags(channels::trailer::CONNECTED, ChannelFlags::EACH_FRAME)?;
        context.subscribe(channels::trailer::CARGO_DAMAGE)?;
        context.subscribe(channels::trailer::WORLD_PLACEMENT)?;
        context.subscribe(channels::trailer::WEAR_BODY)?;
        context.subscribe(channels::trailer::WEAR_CHASSIS)?;
        context.subscribe(channels::trailer::WEAR_WHEELS)?;

        context.subscribe(channels::common::GAME_TIME)?;
        context.subscribe(channels::common::NEXT_REST_STOP)?;
        context.subscribe(channels::common::MULTIPLAYER_TIME_OFFSET)?;
        context.subscribe(channels::truck::HSHIFTER_SLOT)?;
        // Only 2 selectors are ever meaningful (0=splitter, 1=range - see
        // TruckHub's own already-calibrated convention), but subscribed
        // optionally since plenty of gearboxes have neither.
        context.subscribe_at_optional(channels::truck::HSHIFTER_SELECTOR, scs_sdk_plugin::sdk::SdkIndex::new(0).expect("0 is a valid SdkIndex"))?;
        context.subscribe_at_optional(channels::truck::HSHIFTER_SELECTOR, scs_sdk_plugin::sdk::SdkIndex::new(1).expect("1 is a valid SdkIndex"))?;

        // Wheel count varies per vehicle (a car has fewer than a truck with a
        // tag axle; a short trailer fewer than a tridem one) and isn't known
        // until a later `truck`/`trailer` configuration event - too late to
        // subscribe, since subscribing is only valid during `initialize`. So
        // this subscribes up to the games' own fixed limits instead (see
        // `MAX_TRUCK_WHEEL_INDEX`); `subscribe_at_optional` silently skips
        // whatever index a specific vehicle doesn't actually have rather than
        // erroring, and `ensure_wheel_len` (see `handle_channel`) sizes the
        // output array to however many indices actually deliver a value.
        for i in 0..MAX_TRAILER_WHEEL_INDEX {
            let Some(index) = scs_sdk_plugin::sdk::SdkIndex::new(i) else { continue };
            if i < MAX_TRUCK_WHEEL_INDEX {
                context.subscribe_at_optional(channels::truck::WHEEL_SUSP_DEFLECTION, index)?;
                context.subscribe_at_optional(channels::truck::WHEEL_ON_GROUND, index)?;
                context.subscribe_at_optional(channels::truck::WHEEL_SUBSTANCE, index)?;
                context.subscribe_at_optional(channels::truck::WHEEL_VELOCITY, index)?;
                context.subscribe_at_optional(channels::truck::WHEEL_STEERING, index)?;
                context.subscribe_at_optional(channels::truck::WHEEL_ROTATION, index)?;
                context.subscribe_at_optional(channels::truck::WHEEL_LIFT, index)?;
                context.subscribe_at_optional(channels::truck::WHEEL_LIFT_OFFSET, index)?;
            }
            context.subscribe_at_optional(channels::trailer::WHEEL_SUSP_DEFLECTION, index)?;
            context.subscribe_at_optional(channels::trailer::WHEEL_ON_GROUND, index)?;
            context.subscribe_at_optional(channels::trailer::WHEEL_SUBSTANCE, index)?;
            context.subscribe_at_optional(channels::trailer::WHEEL_VELOCITY, index)?;
            context.subscribe_at_optional(channels::trailer::WHEEL_STEERING, index)?;
            context.subscribe_at_optional(channels::trailer::WHEEL_ROTATION, index)?;
            context.subscribe_at_optional(channels::trailer::WHEEL_LIFT, index)?;
            context.subscribe_at_optional(channels::trailer::WHEEL_LIFT_OFFSET, index)?;
        }

        // Must happen before any thread is spawned - see `module_pin`.
        if !module_pin::pin_current_module() {
            context.warning(format_args!(
                "[scs-telemetry-hub] could not pin the plugin module; background threads may outlive its unload"
            ));
        }

        let mut serving = false;
        if !self.http_server_started.swap(true, Ordering::SeqCst) {
            match start_http_server(&self.shared, LISTEN_ADDRESS) {
                Ok(()) => serving = true,
                Err(error) => {
                    self.http_server_started.store(false, Ordering::SeqCst);
                    context.warning(format_args!(
                        "[scs-telemetry-hub] could not start the HTTP server on {LISTEN_ADDRESS}: {error}"
                    ));
                }
            }
        }

        if serving {
            context.message(format_args!(
                "[scs-telemetry-hub] initialized, serving http://{LISTEN_ADDRESS}/snapshot and /stream"
            ));
        } else {
            context.message(format_args!("[scs-telemetry-hub] initialized, HTTP server not running"));
        }
        Ok(())
    }

    fn channel(&mut self, _context: &mut PluginContext<'_>, update: ChannelUpdate<'_>) {
        self.handle_channel(update);
    }

    fn event(&mut self, context: &mut PluginContext<'_>, event: TelemetryEvent<'_>) {
        match event {
            TelemetryEvent::FrameStart(frame) => {
                self.current_render_time.store(frame.render_time(), Ordering::Relaxed);
            }
            TelemetryEvent::FrameEnd => {
                self.detect_refuel();
                self.maybe_broadcast_snapshot();
            }
            TelemetryEvent::Paused => {
                self.shared.snapshot.lock().unwrap_or_else(|e| e.into_inner()).game_paused = true;
            }
            TelemetryEvent::Started => {
                self.shared.snapshot.lock().unwrap_or_else(|e| e.into_inner()).game_paused = false;
            }
            TelemetryEvent::Configuration(configuration_event) => {
                self.handle_configuration(context, configuration_event);
            }
            TelemetryEvent::Gameplay(gameplay_event) => {
                self.handle_gameplay(context, gameplay_event);
            }
        }
    }

    fn shutdown(&mut self, context: &mut PluginContext<'_>) {
        // Clients get a clean end-of-stream right now instead of when the
        // process finally dies, and the port is released so a client retrying
        // in the gap before the game exits is refused rather than accepted.
        self.shared.end_streams();
        self.shared.stop_http_server();
        self.http_server_started.store(false, Ordering::SeqCst);
        context.message(format_args!("[scs-telemetry-hub] shutdown"));
    }
}

export_plugin!(Plugin::default());

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::{Duration, Instant};

    const WAIT: Duration = Duration::from_secs(5);

    /// Starts a real server on an ephemeral port and returns where it landed.
    fn start(shared: &Shared) -> SocketAddr {
        start_http_server(shared, "127.0.0.1:0").expect("binding an ephemeral port should work");
        let server = shared
            .server
            .lock()
            .unwrap()
            .clone()
            .expect("start_http_server stores the server");
        server.server_addr().to_ip().expect("an IP listener")
    }

    /// `Connection: close` makes the server end the connection once the
    /// response is complete, so reading to EOF means "the response finished".
    fn get(addr: SocketAddr, path: &str) -> TcpStream {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream.set_read_timeout(Some(WAIT)).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        stream
    }

    fn stop(shared: &Shared) {
        shared.end_streams();
        shared.stop_http_server();
    }

    #[test]
    fn snapshot_endpoint_serves_json() {
        let shared = Shared::new();
        let addr = start(&shared);

        let mut stream = get(addr, "/snapshot");
        let mut body = String::new();
        stream.read_to_string(&mut body).expect("response should complete");

        assert!(body.contains("200 OK"), "unexpected response: {body}");
        assert!(body.contains("\"type\":\"snapshot\""), "unexpected body: {body}");
        stop(&shared);
    }

    /// Waits for `active_connections` to reach `expected` - i.e. for the
    /// accept loop to have picked the connection up and handed it a thread.
    fn wait_for_connections(shared: &Shared, expected: usize) {
        let deadline = Instant::now() + WAIT;
        while shared.active_connections.load(Ordering::SeqCst) != expected {
            assert!(Instant::now() < deadline, "never reached {expected} active connections");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn stream_ends_cleanly_and_port_is_released_on_shutdown() {
        let shared = Shared::new();
        let addr = start(&shared);

        let mut stream = get(addr, "/stream");
        wait_for_connections(&shared, 1);
        shared.broadcast("{\"type\":\"probe\"}");

        // What `Plugin::shutdown` does.
        stop(&shared);

        // Each message is written and flushed as it's sent, in order, and the
        // connection really ends (EOF) here instead of staying open until the
        // process dies - this one read picks up everything at once simply
        // because nothing read the socket until after `stop` had already run.
        let mut body = String::new();
        stream.read_to_string(&mut body).expect("stream should end, not time out");
        let position = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("missing {needle} in: {body}"));
        let snapshot = position("\"type\":\"snapshot\"");
        let probe = position("\"type\":\"probe\"");
        let disconnected = position("\"type\":\"disconnected\"");
        assert!(snapshot < probe && probe < disconnected, "out of order: {body}");

        // The listening socket gets released, so a retrying client is refused.
        let deadline = Instant::now() + WAIT;
        loop {
            match TcpStream::connect(addr) {
                Err(_) => break,
                Ok(_) if Instant::now() > deadline => panic!("port still accepting connections after shutdown"),
                Ok(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    /// TruckHub's HttpClient keeps its connection alive (no `Connection: close`)
    /// and tells the stream has ended from the final zero-length chunk, not from
    /// a closed socket - so that terminator must arrive on shutdown.
    #[test]
    fn keep_alive_stream_completes_on_shutdown() {
        let shared = Shared::new();
        let addr = start(&shared);

        let mut stream = TcpStream::connect(addr).expect("connect");
        stream.set_read_timeout(Some(WAIT)).unwrap();
        write!(stream, "GET /stream HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        wait_for_connections(&shared, 1);

        stop(&shared);

        let mut received = Vec::new();
        let mut chunk = [0u8; 4096];
        let deadline = Instant::now() + WAIT;
        while !received.ends_with(b"0\r\n\r\n") {
            assert!(Instant::now() < deadline, "no chunk terminator: {}", String::from_utf8_lossy(&received));
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => received.extend_from_slice(&chunk[..n]),
                Err(error) => panic!("read failed before the stream completed: {error}"),
            }
        }
        let text = String::from_utf8_lossy(&received);
        assert!(text.contains("\"type\":\"disconnected\""), "no disconnected message: {text}");
        assert!(received.ends_with(b"0\r\n\r\n"), "response never completed: {text}");
    }

    #[test]
    fn connections_beyond_the_cap_get_503() {
        let shared = Shared::new();
        let addr = start(&shared);

        // Fill every slot with a long-lived stream, waiting for each to be
        // registered so the count is exact before the next one connects.
        let mut held = Vec::new();
        for n in 1..=MAX_HTTP_CONNECTIONS {
            held.push(get(addr, "/stream"));
            wait_for_connections(&shared, n);
        }

        let mut extra = get(addr, "/stream");
        let mut body = String::new();
        extra.read_to_string(&mut body).expect("rejection should complete");
        assert!(body.contains("503"), "expected a 503, got: {body}");
        assert_eq!(shared.active_connections.load(Ordering::SeqCst), MAX_HTTP_CONNECTIONS);

        // Ending the streams frees every slot again.
        stop(&shared);
        for mut stream in held {
            let mut rest = String::new();
            stream.read_to_string(&mut rest).expect("held stream should end on shutdown");
        }
        wait_for_connections(&shared, 0);
    }

    #[test]
    fn snapshot_bypasses_the_connection_cap() {
        let shared = Shared::new();
        let addr = start(&shared);

        // Fill every stream slot first.
        let mut held = Vec::new();
        for n in 1..=MAX_HTTP_CONNECTIONS {
            held.push(get(addr, "/stream"));
            wait_for_connections(&shared, n);
        }

        // /snapshot must still complete, and must not even transiently count
        // against the cap /stream is subject to.
        let mut snapshot = get(addr, "/snapshot");
        let mut body = String::new();
        snapshot
            .read_to_string(&mut body)
            .expect("snapshot should complete even while every stream slot is full");
        assert!(body.contains("200 OK"), "unexpected response: {body}");
        assert_eq!(shared.active_connections.load(Ordering::SeqCst), MAX_HTTP_CONNECTIONS);

        stop(&shared);
        for mut stream in held {
            let mut rest = String::new();
            stream.read_to_string(&mut rest).expect("held stream should end on shutdown");
        }
        wait_for_connections(&shared, 0);
    }

    /// A client that vanishes without a trace (no FIN, no further reads) is
    /// never directly told about by the OS - the only way the server notices
    /// is by trying to write to it. Before the keepalive loop, that only
    /// happened on the next real broadcast, so a dead client with no traffic
    /// held its slot forever; now the periodic ping discovers it within one
    /// `STREAM_KEEPALIVE_INTERVAL`, with no broadcast at all.
    #[test]
    fn dead_stream_client_is_reaped_without_a_broadcast() {
        let shared = Shared::new();
        let addr = start(&shared);

        let stream = get(addr, "/stream");
        wait_for_connections(&shared, 1);
        drop(stream);

        let deadline = Instant::now() + STREAM_KEEPALIVE_INTERVAL + WAIT;
        while shared.active_connections.load(Ordering::SeqCst) != 0 {
            assert!(Instant::now() < deadline, "dead client was never reaped");
            thread::sleep(Duration::from_millis(10));
        }
        stop(&shared);
    }

    /// Exercises `maybe_broadcast_snapshot` directly rather than through
    /// `TelemetryEvent::FrameStart`/`FrameEnd` - those arms are one-line
    /// delegations to `current_render_time`/this method and need no
    /// `PluginContext`, so driving them through the full SDK event-dispatch
    /// machinery would add nothing but a mock to build.
    #[test]
    fn snapshot_broadcast_resumes_after_a_render_time_restart() {
        let plugin = Plugin::default();
        let (tx, rx) = mpsc::channel();
        plugin.shared.subscribers.lock().unwrap_or_else(|e| e.into_inner()).push(tx);

        plugin.current_render_time.store(5_000_000, Ordering::Relaxed);
        plugin.maybe_broadcast_snapshot();
        rx.recv_timeout(WAIT).expect("first broadcast should go out");

        // A save/vehicle/profile reload resets the game's render-time clock.
        plugin.current_render_time.store(100, Ordering::Relaxed);
        plugin.maybe_broadcast_snapshot();
        rx.recv_timeout(WAIT)
            .expect("a backwards render-time jump must not stall broadcasts forever");
    }

    /// Exercises `Shared::apply_trailer_configuration` directly rather than
    /// through `TelemetryEvent::Configuration` - `ConfigurationEvent`'s
    /// constructor is private to the SDK framework crate, so there's no way
    /// to build one here (see that method's own doc comment for why this
    /// split exists).
    #[test]
    fn trailer_configuration_with_attributes_replaces_the_spec() {
        let shared = Shared::new();
        let spec = TrailerSpec { id: "trailer_01".into(), name: "Container Master".into(), ..TrailerSpec::default() };

        shared.apply_trailer_configuration(Some(spec));

        let stored = shared.trailer_spec.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(stored.id, "trailer_01");
        assert_eq!(stored.name, "Container Master");
    }

    #[test]
    fn empty_trailer_configuration_keeps_the_spec_but_invalidates_live_state() {
        let shared = Shared::new();
        let spec = TrailerSpec { id: "trailer_01".into(), name: "Container Master".into(), ..TrailerSpec::default() };
        shared.apply_trailer_configuration(Some(spec));
        {
            let mut snapshot = shared.snapshot.lock().unwrap_or_else(|e| e.into_inner());
            snapshot.trailer.connected = true;
            snapshot.trailer.placement.position = Vec3 { x: 1.0, y: 2.0, z: 3.0 };
        }

        // An empty config - a genuine detach, or (confirmed live) a spurious
        // resend on a menu open/reload while the same trailer is still
        // hooked - must not erase the identity already known for it...
        shared.apply_trailer_configuration(None);
        let stored = shared.trailer_spec.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(stored.id, "trailer_01", "spec must survive an empty config");
        assert_eq!(stored.name, "Container Master", "spec must survive an empty config");
        drop(stored);

        // ...but it must invalidate the live connected/placement state,
        // which the SDK would otherwise leave frozen at its last reading
        // forever with no further signal that it's gone stale.
        let snapshot = shared.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!snapshot.trailer.connected, "connected must be reset - the old value can no longer be trusted");
        let p = snapshot.trailer.placement.position;
        assert_eq!((p.x, p.y, p.z), (0.0, 0.0, 0.0), "placement must be invalidated, not left frozen at its last reading");
    }

    #[cfg(windows)]
    #[test]
    fn module_pin_succeeds() {
        assert!(module_pin::pin_current_module());
    }
}
