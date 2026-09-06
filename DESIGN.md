# fehu — design

`fehu` is a `no_std + alloc` Rust library that simulates a synthetic share price
for a game. External inputs (game state, player actions) move a slow
*fundamental value*; a noisy *price* mean-reverts toward it with clustered
volatility and occasional jumps. Everything is deterministic given a seed and an
event stream, and the full state can be saved and restored.

This document fixes the model, the parameterisation, the per-tick algorithm, the
state layout, the public API and the test plan **before** any code is written.
Section 13 lists the decisions I want confirmed, several of which go slightly
beyond the letter of the request.

---

## 1. Goals and non-goals

Goals

- Layered model: fundamental → mean-reverting price → GARCH(1,1) vol → Poisson jumps.
- All rates annualised in config; the crate scales them to the tick.
- Bit-identical output across native and `wasm32` for the same seed and events.
- `i64` cents at the boundary, `f64` latent state inside, no price ≤ 0.
- Full `serde` round-trip of simulator state.
- Cheap: millions of ticks per second on native.

Non-goals

- Mapping game actions to events (the game does that; the crate only consumes `Event`).
- Order books, multiple assets, real calendars with holidays/time zones (a simple
  UTC weekday calendar is provided; holidays can be added later).
- Statistical realism beyond "looks and feels like a stock": fat tails, vol
  clustering, gaps, reversion after shocks.

---

## 2. Crate shape

```
fehu/
  Cargo.toml          edition = "2024", no default features except "std"
  DESIGN.md
  justfile
  src/
    lib.rs            #![no_std], extern crate alloc, re-exports
    math.rs           libm wrappers; uniform / normal / poisson samplers
    time.rs           Timestamp, MarketHours calendar arithmetic
    config.rs         Config, GarchParams, JumpParams, VolumeParams, validation, Derived
    event.rs          Event, EventKind, decaying effect bookkeeping
    sim.rs            Simulator, Tick, step / advance
    candles.rs        Candles, Candle, Interval
  tests/              integration tests (see §11)
  benches/ticks.rs    criterion, ticks/sec
  examples/dump.rs    one year of 1 s ticks + daily candles → CSV (needs "std")
```

Features

| feature | default | effect |
|---|---|---|
| `std`   | yes | `std::error::Error` impls; enables the example. Core code never needs it. |
| `serde` | no  | `Serialize`/`Deserialize` on `Config`, `Event`, `Tick`, `Candle`, `Candles`, `Simulator` (incl. RNG state). |

Dependencies: `libm`, `rand_core`, `rand_xoshiro` (+ its serde feature under our
`serde` feature), `serde` (optional, `derive`, no `std`). Dev: `proptest`,
`criterion`, `serde_json`.

---

## 3. Time

### 3.1 Wall time

- `Timestamp(i64)` — **milliseconds since the Unix epoch, UTC**. Plain integer,
  cheap to serialise, easy to consume from JS/WASM.
- `Config::tick: Duration` — wall time between ticks. Default `1 s`. Must be a
  whole number of milliseconds, `1 ms ≤ tick ≤ 1 day`.
- `Config::start_ts: Timestamp` — timestamp of the first tick. Default `0`.
  (1970-01-01 is a Thursday; the calendar below uses that.)
- No wall clock is ever read; the simulator's clock advances only through
  `step`/`advance`.

### 3.2 Model time

Annualised parameters are converted to per-step quantities through the
**model-time year length `Y`** (in model-seconds):

| mode | `Y` (model-seconds / year) |
|---|---|
| no market hours | `365.25 · 86 400` |
| market hours    | `D · S · (1 + g)` with `D = 365.25 · n_days / 7` trading days/yr, `S = close − open` seconds, `g` = overnight gap weight |

A step covering `m` model-seconds has `Δ = m / Y` years. The regular tick has
`m = tick_s`, hence `dt = tick_s / Y`.

**Half-lives given as `Duration` (event shifts, GARCH persistence) are in
model-seconds.** Without market hours model time equals wall time. With market
hours, model time runs during sessions and each overnight/weekend gap counts as
`g · S` model-seconds, so an effect with a 2 h half-life decays a little (not a
full night's worth) while the market is closed.

### 3.3 Market hours (optional)

```rust
pub struct MarketHours {
    pub open_secs:  u32,   // seconds after 00:00 UTC, default 34 200 (09:30)
    pub close_secs: u32,   // default 57 600 (16:00); open < close ≤ 86 400
    pub weekdays:   u8,    // bit 0 = Monday … bit 6 = Sunday; default 0b001_1111
    pub gap_weight: f64,   // g, variance of one closed period relative to a full
                           // session; default 0.30, range [0, 2]
}
```

- Ticks are generated at `open, open + tick, …, < close` on trading days only.
- Weekday of a timestamp: `(days_since_epoch + 3).rem_euclid(7)` (0 = Monday).
- The first tick of a session **covers the closed period plus its own tick**:
  its model-time is `g·S + tick_s`. Consequently one trading day sums to exactly
  `S·(1+g)` model-seconds `= 1/D` year, so the realised annualised vol equals the
  configured `σ` including gaps. The very first tick after `Simulator::new` has
  no gap (nothing to gap from).
- If `start_ts` is outside a session, the first tick is at the next open.
- Every gap is weighted the same (`g·S`) regardless of its wall length; a
  weekend is not 3× an overnight. That matches the empirical fact that weekend
  variance is only mildly larger than overnight variance and keeps the
  annualisation exact.

---

## 4. Model

Notation: `P` price, `V` fundamental value, `f = ln V`, `p = ln P`,
`s = p − f` the **log-spread**. Everything annualised unless stated. `Δ` is the
step length in years, `z ~ N(0,1)`.

### 4.1 Layer 1 — fundamental value

Deterministic given inputs. Two pieces of state: the target `f*` and the current
`f`, which relaxes toward the target.

```
f*  ← f* + μ Δ                            long-run growth (Config::drift)
f*  ← f* + δ         on FundamentalShift(δ)
f*  ← ln(target_cents / 100)   on set_fundamental_input(target_cents)
f   ← f + (f* − f)(1 − e^{−κ_f Δ})        exact relaxation, κ_f = Config::fundamental_speed
```

`κ_f` default `36 /yr` (half-life ≈ 7 model-days). `κ_f = 0` makes `f` follow
`f*` only through the drift term (never relaxing), which is allowed.

### 4.2 Layer 2 — price, OU on the log-spread

```
ds = −θ s dt + D(t) dt + σ_t dW + jumps
```

- `θ` = `Config::mean_reversion_speed`, default `50 /yr` (spread half-life
  `ln2/θ ≈ 5.1` model-days).
- `D(t)` = superposed drift-shift effects (§4.5), zero by default. `Config::drift`
  deliberately does **not** enter here: a constant drift on a mean-reverting
  spread would only produce a constant offset `μ/θ`. Long-run growth belongs to
  the fundamental.
- `σ_t` = effective annualised vol (§4.3 + §4.5).

Exact discretisation over a step `Δ` (stable for any `Δ`, including gaps):

```
s' = s·e^{−θΔ}
   + Σ_i a_i (e^{−λ_i Δ} − e^{−θΔ}) / (θ − λ_i)   exact solution of ds = −θs dt + a e^{−λt} dt
                                                (a_i Δ e^{−θΔ} when θ ≈ λ_i)
   + σ_t · sqrt((1 − e^{−2θΔ}) / (2θ)) · z       OU noise (→ σ_t·sqrtΔ as θΔ → 0)
   + Σ_k J_k                                     jumps in this step
p' = f' + s'
```

The stationary spread std is `σ/sqrt(2θ)`; with defaults `0.40/sqrt(100) = 4 %`.

Because noise, jumps and drift shifts all live on the spread, **every shock
reverts toward the fundamental** with half-life `ln2/θ`. Only `FundamentalShift`
and `set_fundamental_input` move the price permanently.

### 4.3 Layer 3 — GARCH(1,1) variance

Per-tick variance `h_t` of the diffusive innovation, in the classic form

```
h_{t+1} = ω + α ε_t² + β h_t,      ε_t = sqrt(h_t) · z_t
```

Classic `(α, β)` are per-tick numbers whose meaning changes with `tick`, which
contradicts "annualise everything". `fehu` therefore parameterises the
continuous-time limit (Nelson 1990) and derives `(α, β, ω)` for the actual tick:

```rust
pub struct GarchParams {
    pub variance_half_life: Duration, // h_v, model-seconds, default 1 h
    pub variance_dispersion: f64,     // r ∈ [0, 1), default 0.5
}
```

```
κ_v = ln2 · Y / h_v_secs                 variance mean-reversion, /yr
q   = κ_v · dt
α   = sqrt(r · q)                       (Nelson: α = ν·sqrt(dt/2), ν² = 2 r κ_v)
β   = 1 − q − α
ω   = q · σ² · dt                       so that E[h] = σ² dt
```

Properties (Gaussian innovations):

- persistence `α + β = 1 − q`, half-life of a variance shock = `h_v` exactly;
- `Var(h)/E[h]² = r / (1 − r)`; with `r = 0.5` the per-tick variance has a
  standard deviation equal to its mean — visible calm/panic regimes;
- unconditional kurtosis of returns shorter than `h_v` is `3 / (1 − r)` = 6 by
  default, so "kurtosis > 3 on 1 m returns" holds even without jumps;
- finite fourth moment ⇔ `r < 1`; `β ≥ 0` ⇔ `q + sqrt(r q) ≤ 1`, i.e. the tick
  must be short relative to the variance half-life. Validation rejects
  otherwise ("tick too coarse for `variance_half_life`").

Defaults at 1 s ticks, no market hours: `q ≈ 1.93e-4`, `α ≈ 0.0098`,
`β ≈ 0.990`. `GarchParams::from_alpha_beta(α, β, tick)` and
`GarchParams::alpha_beta(&self, dt)` convert to/from the classic form.

The GARCH recursion uses only the standardised diffusive shock `z_t²`
(`ε² = h_t z²`), so it is the same for the gap step. **Jumps and vol-shift events
do not feed `h_t`.** Feeding a −20 % jump into a per-second GARCH would blow the
variance up by orders of magnitude; a game that wants "panic after a shock"
pushes a `VolShift` alongside the `Jump`.

Effective annualised vol used by the price step:

```
σ_t = max(0, sqrt(h_t / dt) + Σ_j v_j(t))      v_j = decaying vol-shift effects
```

### 4.4 Layer 4 — Poisson jumps (Merton)

```rust
pub struct JumpParams {
    pub intensity: f64, // λ, jumps per year, default 100 (≈ 2 / week), range [0, 1e4]
    pub mean: f64,      // μ_J, mean log-jump, default −0.005, range [−1, 1]
    pub std: f64,       // σ_J, default 0.03, range [0, 1]
}
```

Per step: `N ~ Poisson(λΔ)` by inversion (`p = e^{−λΔ}`, cumulate until the
uniform is exceeded), then `N` draws `J_k ~ N(μ_J, σ_J²)` added to `s`. No
compensator is applied: jumps sit on the spread and revert, so they do not bias
long-run growth. `λ · Δ ≤ 30` is guaranteed by the validated ranges.

`Config::volatility` is the *diffusive* vol. Total short-horizon variance is
`σ² + λ (μ_J² + σ_J²)`; with defaults `0.16 + 0.0925`, i.e. realised annualised
vol ≈ 0.50 rather than 0.40. Set `intensity = 0` to make them coincide.

### 4.5 Events and decay

```rust
pub struct Event { pub at: Timestamp, pub kind: EventKind }
pub enum EventKind {
    Jump(f64),                                       // pct: −1 < pct, e.g. 0.10 = +10 %
    DriftShift { delta: f64, half_life: Duration },  // Δ annualised drift on the spread
    VolShift   { delta: f64, half_life: Duration },  // Δ annualised vol, may be negative
    FundamentalShift(f64),                           // δ added to f* (log units; ×e^δ)
}
```

- Events are queued in a min-heap keyed by `(at, sequence)`; equal timestamps
  apply in push order. An event is applied at the start of the first tick with
  `ts ≥ at`. Events with `at` in the past apply on the next tick. An event
  falling in a closed period applies at the next open, before the gap draw, so
  the opening print already reflects it.
- `Jump(pct)`: `s ← s + ln(1 + pct)` immediately (reverts, see §4.2).
- `FundamentalShift(δ)`: `f* ← f* + δ`; `f` and then `p` follow with the two
  lags `κ_f` and `θ`. Permanent.
- `DriftShift`/`VolShift` create a **decaying effect** `a(t) = delta · 2^{−τ/h}`
  where `τ` is model time since application. Effects **superpose additively**
  and are stored as a list `(amplitude, λ = ln2 / h_secs)`; each step multiplies
  every amplitude by `e^{−λ m}` (`m` = the step's model-seconds) and prunes
  `|a| < 1e-12`. Cost is O(active effects) per tick.
- Drift shifts enter the spread through the exact solution of the forced OU
  equation over the step (§4.2), so the discrete series matches the closed
  form `s(t) = delta · (e^{−λt} − e^{−θt}) / (θ − λ)` to floating-point
  precision; it peaks at `t* = ln(θ/λ) / (θ − λ)`. A shift of `delta` with
  half-life `h` (years) moves `ln P` by a total of `delta · h / ln2` if `θ`
  were zero. Helper `EventKind::drift_for_total_move(total, half_life)`
  computes `delta = total · ln2 / h` (calendar year) so game code can think in
  "move 5 % over about an hour".
- Vol shifts add to `σ_t` at the value at the start of the step.

### 4.6 Overnight gap

The first tick of a session is a single OU step of `Δ_gap + dt` years,
`Δ_gap = g / (D·(1+g))`, using the current `h_t` scaled to that length, the
current drift/vol effects, and Poisson jumps with intensity `λ(Δ_gap + dt)`.
The fundamental and all decays advance by the same model time. Nothing else is
special about it.

### 4.7 Volume

`Tick::volume: u64` is derived, not modelled:

```
base_tick = base_per_day · tick_s / S_day             S_day = S (market hours) or 86 400
regime    = (σ_t / σ)^γ                               current vol regime vs unconditional
activity  = 1 + c · |r_t| / (σ · sqrt(dt))            this tick's move vs a normal tick's std
E[vol]    = base_tick · regime · activity
volume    = round( E[vol] · exp(η z_v − η²/2) )       lognormal noise, mean-preserving
```

```rust
pub struct VolumeParams {
    pub base_per_day: f64,        // default 1_000_000 shares, range (0, 1e15]
    pub return_sensitivity: f64,  // c, default 2.0, range [0, 100]
    pub vol_exponent: f64,        // γ, default 1.0, range [0, 4]
    pub noise: f64,               // η, default 0.5, range [0, 3]
}
```

At the open the gap return is compared with a *regular* tick's std, so the
opening print naturally carries a few hundred ticks' worth of volume (≈ 1 % of
the day with defaults for a 1 % gap).

Because `E|r| / (σ√dt) = sqrt(2/π)` for Gaussian ticks, the mean daily volume
is `base_per_day · (1 + c · sqrt(2/π))` ≈ `2.6 · base_per_day` with defaults
(`base_per_day` is the volume of a day with no price movement).

### 4.8 Output and guards

```
price_cents = clamp( round( 100 · e^{p} ), 1, 10^15 )
```

- Latent `p` is clamped to `[ln 0.005, ln 10^13]` after every step so the
  process can never get stuck at zero or overflow `i64`; the spread absorbs the
  clamp (`s = p − f`).
- Config validation guarantees every parameter is finite and in range, so `NaN`
  cannot arise; a debug assertion checks `p.is_finite()` anyway.

---

## 5. Per-tick algorithm and RNG draw order

`step()` performs, in this order (the order is part of the determinism contract
and is frozen by `STATE_VERSION`):

1. Decide the tick timestamp `ts` (next tick, or next session open) and its
   model-seconds `m` (`tick_s`, or `g·S + tick_s` for a session's first tick
   when a previous session exists). `Δ = m / Y`.
2. Pop and apply all events with `at ≤ ts`.
3. Fundamental: `f* += μΔ`; `f_old = f`; `f += (f* − f)(1 − e^{−κ_f Δ})`.
4. Drift/vol effects: compute the drift integral `I = Σ a_i (1−e^{−λ_i Δ})/λ_i`
   and vol add `V = Σ v_j`; then decay every amplitude by `e^{−λ m}` and prune.
5. `σ_t = max(0, sqrt(h/dt) + V)`.
6. Draw `z` (normal). `s' = s e^{−θΔ} + I + σ_t sqrt((1−e^{−2θΔ})/(2θ)) z`.
7. Draw `u` (uniform) → `N ~ Poisson(λΔ)`; for each of `N`: draw `J` (normal)
   → `s' += μ_J + σ_J J`.
8. GARCH: `h ← ω + α h z² + β h`.
9. `p_old = p`; `p = f + s'`, clamp; `s = p − f`.
10. Volume: draw `z_v` (normal); compute as §4.7 with `r = p − p_old`.
11. Emit `Tick { ts, price_cents, volume }`; set `clock = ts`; schedule the next
    tick timestamp.

RNG: `Xoshiro256PlusPlus::seed_from_u64(seed)` (SplitMix64 expansion, fixed by
`rand_core`). Only `next_u64` is ever called. Conversions are our own:

- uniform `[0,1)`: `(x >> 11) as f64 · 2^{−53}`; `(0,1]` variant adds one ulp.
- normal: Box–Muller, `sqrt(−2 ln u1) · cos(2π u2)`, second branch discarded
  (no cached spare, so no extra state). Two `u64` per normal.
- Poisson: inversion as in §4.4, one uniform.

Draws per regular tick: 1 normal (price) + 1 uniform (jump count) + N normals
(jumps) + 1 normal (volume). ~7 libm calls per tick; target ≥ 2 M ticks/s native.

---

## 6. Parameters

| symbol | `Config` field | unit | default | valid range |
|---|---|---|---|---|
| `P_0` | `start_price_cents: i64` | cents | `10_000` ($100) | `[1, 10^15]` |
| `μ` | `drift: f64` | /yr (log growth of `V`) | `0.05` | `[−2, 2]` |
| `σ` | `volatility: f64` | /√yr | `0.40` | `[0, 5]` |
| `θ` | `mean_reversion_speed: f64` | /yr | `50` (½-life ≈ 5 d) | `(0, 10^5]` |
| `κ_f` | `fundamental_speed: f64` | /yr | `36` (½-life ≈ 7 d) | `[0, 10^5]` |
| `h_v`, `r` | `garch: GarchParams` | see §4.3 | `1 h`, `0.5` | `h_v ∈ [1 s, 1 yr]`, `r ∈ [0, 1)`, and `β ≥ 0` |
| `λ, μ_J, σ_J` | `jumps: JumpParams` | see §4.4 | `100, −0.005, 0.03` | see §4.4 |
| — | `volume: VolumeParams` | see §4.7 | see §4.7 | see §4.7 |
| `tick_s` | `tick: Duration` | wall | `1 s` | `[1 ms, 1 d]`, whole ms |
| — | `start_ts: Timestamp` | ms | `0` | any |
| — | `market_hours: Option<MarketHours>` | see §3.3 | `None` | `open < close ≤ 86400`, `weekdays ≠ 0`, `tick ≤ S`, `g ∈ [0,2]` |

Derived per-tick quantities (struct `Derived`, recomputed from `Config` on
construction and after deserialisation, never edited by hand):

```
Y, D, S, dt = tick_s / Y, Δ_gap = g/(D(1+g)) (0 without market hours)
q = ln2 · tick_s / h_v_secs,  α = sqrt(r q),  β = 1 − q − α,  ω = q σ² dt
tick_std = σ sqrt(dt)         (for volume normalisation)
e_theta_tick = e^{−θ dt}, ou_noise_tick = sqrt((1−e^{−2θ dt})/(2θ))   (cached for the regular tick)
```

`Config::validate() -> Result<(), ConfigError>` checks all of the above;
`ConfigError` is an enum naming the offending field and reason.

---

## 7. State layout (what gets serialised)

```rust
pub struct Simulator {                // serde(try_from = "SimulatorRepr", into = "SimulatorRepr")
    config: Config,
    derived: Derived,                 // not serialised, rebuilt on load
    rng: Xoshiro256PlusPlus,          // 4 × u64
    clock: Timestamp,                 // wall time the simulator has been advanced to
    next_ts: Timestamp,               // timestamp of the next tick to emit
    last_ts: Option<Timestamp>,       // detects a session boundary (gap)
    log_price: f64,                   // p
    log_fund: f64,                    // f
    log_fund_target: f64,             // f*
    variance: f64,                    // h, per-tick GARCH variance
    last_vol: f64,                    // σ_t of the last step, for Snapshot
    drift_effects: Vec<Decaying>,     // { amplitude, lambda } lambda per model-second
    vol_effects: Vec<Decaying>,
    pending: BinaryHeap<Reverse<Queued>>, // Queued { at, seq, kind }; a Vec in the repr
    next_seq: u64,
    candle_cursor: [CandleBuilder; 4], // in-progress bars for the candle-stream API (§8.4)
}

pub struct SimulatorRepr { pub version: u32, /* the fields above minus `derived` */ }
```

`TryFrom<SimulatorRepr>` checks `version == STATE_VERSION`, re-validates the
config, rebuilds `Derived` and rejects non-finite latent values (`LoadError`).

Everything is plain numbers, so `serde_json`, `postcard`, `bincode` etc. all
work. Binary formats round-trip `f64` bit-exactly. **JSON needs `serde_json`'s
`float_roundtrip` feature**; without it a parsed float can be one ulp off, which
the determinism tests would catch.

`Candles` state is separate (§8.3) and serialisable on its own; the save game
stores both or rebuilds candles from stored ticks.

---

## 8. Public API

### 8.1 Types

```rust
pub struct Timestamp(pub i64);            // ms since epoch, UTC; ± Duration, Timestamp − Timestamp → ms
pub struct Tick { pub ts: Timestamp, pub price_cents: i64, pub volume: u64 }
pub struct Config { ... §6 ... }          // impl Default; fn validate(&self)
pub struct Event { pub at: Timestamp, pub kind: EventKind }
pub enum EventKind { Jump(f64), DriftShift{..}, VolShift{..}, FundamentalShift(f64) }
pub enum ConfigError { NotFinite { field }, OutOfRange { field, reason } }  // #[non_exhaustive]
pub enum EventError  { NotFinite, JumpBelowMinusOne, ZeroHalfLife }       // from push_event
pub enum LoadError   { VersionMismatch { found, expected }, Config(ConfigError), Corrupt }
// all implement Display, and std::error::Error under "std"
pub struct Snapshot {                     // read-only view of latent state, for tests/UI
    pub ts: Timestamp, pub price_cents: i64, pub fundamental_cents: i64,
    pub log_spread: f64, pub annual_vol: f64, pub drift_effect: f64, pub vol_effect: f64,
    pub pending_events: usize,
}
```

### 8.2 Simulator

```rust
impl Simulator {
    pub fn new(config: Config, seed: u64) -> Result<Self, ConfigError>;
    pub fn config(&self) -> &Config;
    pub fn step(&mut self) -> Tick;                                   // exactly one tick
    pub fn advance(&mut self, dur: Duration) -> impl Iterator<Item = Tick> + '_;
    pub fn push_event(&mut self, event: Event) -> Result<(), EventError>;
    pub fn set_fundamental_input(&mut self, target_cents: i64);        // f* ← ln(target/100)
    pub fn clock(&self) -> Timestamp;
    pub fn next_tick_ts(&self) -> Timestamp;
    pub fn snapshot(&self) -> Snapshot;
}
```

- `advance(dur)`: sets `clock += dur` up front and lazily yields every tick with
  `ts ≤ clock`. Dropping the iterator early is safe; the remaining ticks come
  out of the next `advance`/`step`. With market hours a `dur` spanning a closed
  period yields fewer ticks.
- `advance` is inclusive: a tick at exactly `clock + dur` is emitted.
- `step()` always emits the next tick and moves `clock` to it (never backwards).
- `push_event` validates the event (finite magnitudes, `Jump > −1`, positive
  half-life) and returns `Err` rather than poisoning the state with `NaN`.
- `set_fundamental_input` takes cents rather than a log level so game code
  never touches logs. A second entry point `set_fundamental_growth(rate)`
  overriding `Config::drift` at runtime is cheap to add if wanted (§13).

### 8.3 Candles

```rust
pub enum Interval { M1, M5, H1, D1 }   // also `Interval::millis(&self) -> i64`
pub struct Candle {
    pub open_ts: Timestamp, pub open: i64, pub high: i64, pub low: i64, pub close: i64,
    pub volume: u64, pub ticks: u32,
}
pub struct Candles { .. }
impl Candles {
    pub fn new(max_per_interval: usize) -> Self;       // ring buffers, oldest evicted
    pub fn push(&mut self, tick: &Tick) -> [Option<Candle>; 4]; // candles that just closed
    pub fn completed(&self, iv: Interval) -> impl ExactSizeIterator<Item = &Candle>;
    pub fn current(&self, iv: Interval) -> Option<&Candle>;     // partial, in-progress
}
// also: Interval::ALL, Interval::millis(), Interval::bucket(ts)
```

Buckets are aligned to epoch multiples of the interval (`open_ts = ts − ts mod
interval`). With market hours a 1 d candle spans the session (ticks only exist
inside it); no empty candles are emitted for closed periods.

### 8.4 Candle streams

For callers that only want bars, the simulator can drive the tick loop itself:

```rust
impl Simulator {
    /// Runs ticks internally and yields each candle of `iv` as it closes.
    pub fn candles(&mut self, iv: Interval) -> impl Iterator<Item = Candle> + '_;
    /// Same, bounded by wall time: yields the candles that close within `dur`.
    pub fn advance_candles(&mut self, dur: Duration, iv: Interval)
        -> impl Iterator<Item = Candle> + '_;
}
```

Both are thin adapters over `step`/`advance` and `Candles`, so the candles are
bit-identical to aggregating the same ticks by hand, and the simulator's clock
and RNG advance exactly as they would through `step`. A partially built candle
is not lost on drop: the next call continues it, because the aggregator lives
inside the iterator's borrowed `Simulator` state (`candle_cursor: Option<Candles>`
in §7).

Not designed here: a coarse mode that steps once per candle and synthesises
high/low from the Brownian-bridge extremum distribution. It would make years of
daily history almost free but is a second code path; add later if needed.

---

## 9. Numerics and determinism rules

1. **All transcendental math goes through `crate::math`**, which wraps
   `libm::{exp, log, log1p, sqrt, pow, cos, round}`. No `f64::exp` etc. — they
   are `std`-only, so the `no_std` build enforces this mechanically. `powi` and
   `mul_add` are also banned (LLVM intrinsics; may differ per target).
2. Rust never contracts `a*b+c` into FMA, and `x86_64`/`wasm32` both use IEEE
   binary64 without extended precision, so plain arithmetic is bit-identical.
3. Only `next_u64` from the RNG; all float conversion is ours (§5).
4. No `HashMap` iteration order anywhere; event ordering is by `(at, seq)`.
5. No wall clock, no globals, no `rand::thread_rng`.
6. `STATE_VERSION` is bumped whenever the draw order or state layout changes;
   golden tests pin the current output.

---

## 10. Tick-scaling cheat sheet

| quantity | formula |
|---|---|
| step length | `Δ = m / Y` years, `m` model-seconds |
| OU decay | `e^{−θΔ}` |
| OU noise std | `σ_t sqrt((1 − e^{−2θΔ}) / 2θ)` |
| fundamental relaxation | `1 − e^{−κ_f Δ}` |
| effect decay | `e^{−(ln2 / h_secs) · m}` |
| effect drift integral | `a (1 − e^{−λΔ}) / λ` with `λ` per year |
| GARCH | `q = ln2·tick_s/h_v_secs`, `α = sqrt(r q)`, `β = 1−q−α`, `ω = q σ² dt` |
| jump count | `Poisson(λΔ)` |
| volume base | `base_per_day · tick_s / S_day` |

---

## 11. Test plan

Integration tests under `tests/`, each fast enough for CI (< ~2 s each):

| test | method |
|---|---|
| determinism | two simulators, same seed/config/events, 100 k ticks → identical `Tick`s. Plus a **golden FNV-1a hash** of 10 k ticks with `Config::default()`, seed 42, asserted equal under `--no-default-features` and `--features std,serde` (the `just test` recipe runs both). |
| positivity | proptest configs × 5 k ticks: every `price_cents ≥ 1`, finite latent state. Includes adversarial configs (σ = 5, huge negative jumps, `Jump(−0.999)`). |
| fat tails | 10 days of 1 s ticks → 1 m log returns → sample kurtosis `> 3.5`. |
| vol clustering | same series: autocorrelation of `|r_1m|` at lags 1..10 all `> 0` (expected ≈ 0.2–0.3, se ≈ 0.008). |
| mean-reversion half-life | (a) deterministic: `σ = 0, λ = 0`, `Jump(0.10)` at start, tick = 1 min; time for the log-spread to halve equals `ln2/θ` ± 1 tick. (b) stochastic: tick = 1 h, `θ = 500`, 20 k ticks, AR(1) fit of the spread → `θ̂` within 25 %. |
| events | with `σ = 0, λ = 0`: `Jump(p)` → next price `= P·(1+p)` within 1 cent, then reverts; `FundamentalShift(δ)` → after many half-lives price `= P e^δ` within 0.1 %; `DriftShift` → series matches the closed form §4.5 within 1e-9; `drift_for_total_move` integrates to `total`; `VolShift` (stochastic, same seed vs control) → realised-vol ratio within 5 % of `sqrt(mean σ_t²)/σ`; superposition and pruning; push order at equal timestamps; invalid events rejected. |
| save/load | run under market hours with pending events, effects and a partial candle; JSON (`float_roundtrip`) and `postcard` round trips continue identically for 30 k ticks; `Candles` round-trips; wrong `STATE_VERSION` and invalid config are rejected. |
| market hours | ticks only inside sessions; first tick of a day carries the gap; per-day tick count `= S / tick`; weekend skipped; realised annualised vol over 60 sessions within 15 % of `σ`. |
| candles | hand-built ticks → OHLCV correct, bucket alignment, eviction at `max_per_interval`. |
| config fuzzing | proptest: random fields in-range → `validate` Ok and 1 k ticks run; out-of-range → `Err` naming that field. |

Statistical tests use fixed seeds so they never flake.

---

## 12. Tooling

`justfile`

```
build   cargo build --all-features; --no-default-features; --no-default-features --features serde
test    cargo test --all-features && cargo test --no-default-features --tests
lint    cargo fmt --all -- --check && cargo clippy --all-targets (all-features and no-default-features) -- -D warnings
bench   cargo bench --bench ticks         (criterion: ticks/sec default, market hours, advance + candles)
wasm    rustup target add wasm32-unknown-unknown && cargo build --release --target wasm32-unknown-unknown --no-default-features [--features serde]
dump    cargo run --release --example dump -- OUT_DIR SEED
ci      lint build test wasm
```

Measured on the development container: ≈ 5 M ticks/s in the bench, and the
example writes a year of 1 s ticks (31.5 M rows) plus daily candles in ≈ 13 s
including CSV output.

`examples/dump.rs` (requires `std`): default config, seed from argv, writes
`ticks.csv` (`ts,price_cents,volume`, one year of 1 s ticks ≈ 31.5 M rows) and
`daily.csv` (OHLCV) to an output directory given on the command line.

Commit order (small commits): scaffold + `DESIGN.md` → `math`/`time`/`config` →
core price process → GARCH → events → candles & volume → market hours → tests
→ bench + example + justfile.

---

## 13. Decisions (confirmed at review)

1. **Extra `Config` fields** beyond the request: `fundamental_speed`, `volume`,
   `start_ts`. All have defaults.
2. **GARCH parameterisation** is `(variance_half_life, variance_dispersion)`
   rather than `(α, β)`, to stay tick-invariant; classic converters provided.
3. **`Config::drift` grows the fundamental**, not the price process; drift
   shifts push the spread and therefore revert.
4. **All shocks revert**: `Jump` and random Poisson jumps live on the spread.
   Permanent moves come only from `FundamentalShift`/`set_fundamental_input`.
5. **Jumps do not feed GARCH** (§4.3 explains why).
6. **Half-lives are `Duration`s in model time** (= wall time without market
   hours); with market hours a closed period counts as `g·S` model-seconds.
7. **`Simulator::new` returns `Result`** so invalid configs are an error, not a
   panic.
8. **`Timestamp` is `i64` milliseconds**, and `tick` must be whole milliseconds.
9. **Overnight gap** is one OU step of `Δ_gap = g/(D(1+g))` years merged into the
   opening tick, with every gap weighted equally.
10. `set_fundamental_input` takes a **target price in cents**; growth stays in
    `Config::drift`.
11. Added during implementation: `push_event` returns `Result<(), EventError>`;
    the drift-shift integral uses the exact forced-OU solution (§4.2); JSON
    saves need `serde_json/float_roundtrip` for bit-exactness (§7).
