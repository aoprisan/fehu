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
- Multiple assets, real calendars with holidays/time zones (a simple UTC
  weekday calendar is provided; holidays can be added later). An order book
  *is* provided, layered on top of the price process (§14).
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
    book.rs           OrderBook: price–time priority matching (§14)
    exchange.rs       Exchange: simulator + synthetic liquidity + trader impact (§14)
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

### 8.5 Coarse mode

```rust
impl Simulator {
    /// One latent step per candle; endless.
    pub fn coarse_candles(&mut self, iv: Interval) -> impl Iterator<Item = Candle> + '_;
    /// Coarse bars of `iv` that close within `dur`.
    pub fn advance_coarse(&mut self, dur: Duration, iv: Interval) -> impl Iterator<Item = Candle> + '_;
}
```

For long histories (years of daily bars) the tick path is wasteful. Coarse
mode runs **steps 2–9 of §5 once per bar** over the whole bar's model time and
synthesises the intra-bar extremes:

- **Step geometry.** The bar covers the wall interval `[a, b)` from the current
  position `a` to the end of its bucket, clipped to the session close under
  market hours; `b` past the close parks the simulator at the next open, and
  the first bar of a session also carries the gap (`g·S`). A first bar may be
  partial if `a` is not on a bucket boundary. `Candle::ticks = 0` marks a
  coarse bar.
- **Same latent evolution.** Fundamental relaxation, exact OU step, drift/vol
  effect integrals, Poisson jumps with `λΔ`, events with `at < b`: identical
  code (`Simulator::evolve`), so events, reversion and vol shifts behave the
  same way at any step length.
- **GARCH at the step scale.** The per-tick `(α, β)` are meaningless for a
  step longer than the variance half-life, so the coefficients are derived
  per step from the same two invariants (persistence and dispersion):
  `φ = e^{−ln2 · m / h_v}`, `α = min(sqrt(r (1 − φ²) / 2), φ)`, `β = φ − α`,
  `ω = (1 − φ) σ² dt` (per-tick units, since `h` is stored per tick and
  `σ_t = sqrt(h/dt)` is scale-free). For `m ≪ h_v` this reduces to the fine
  formulas; for `m ≫ h_v` the regime averages out (`φ → 0`) and daily returns
  are close to Gaussian, which is physically right. The `min(·, φ)` keeps
  `β ≥ 0` and shrinks dispersion gracefully instead of rejecting the config.
- **High/low: Brownian-bridge extremum.** Given the bar's log move
  `x = ln(close/open)` and total variance `v = σ_t² Δ + λΔ(μ_J² + σ_J²)`, the
  maximum `M` of a Brownian bridge from 0 to `x` has
  `P(M ≥ m) = exp(−2 m (m − x) / v)`, so with a uniform `u`
  `m = (x + sqrt(x² − 2 v ln u)) / 2`; the minimum is the mirror image
  `(x − sqrt(x² − 2 v ln u')) / 2` with an independent `u'`. The two draws are
  independent (the exact joint law is an infinite series), and reversion inside
  the bar is ignored. `high ≥ max(open, close)` and `low ≤ min(open, close)`
  hold by construction. With `v = 0` the extremes are the endpoints.
- **Volume.** The tick formula at the bar scale:
  `base_per_day · s/S_day · (σ_t/σ)^γ · (1 + c |x| / (σ√Δ))` with `s` the
  session seconds in the bar, whose expectation equals the aggregate of the
  ticks it replaces; the lognormal noise std is `η / sqrt(ticks in bar)`.
- **Draw order per bar:** `z`, `u_poisson`, jump normals, `u_hi`, `u_lo`,
  `z_v`. Coarse bars are therefore *not* the aggregate of the tick path for
  the same seed — they are a separate, statistically equivalent path — but
  they are just as deterministic. Mixing coarse and fine calls on one
  simulator is allowed; the state continues from wherever the last call
  stopped (switching coarse → fine overlaps the model time by at most one
  tick).

Validated in `tests/coarse.rs`: daily std and mean high–low range of coarse
bars against both the analytic BM range `2 sqrt(2/π) σ √T` (within 5 %) and a
1-minute fine simulation aggregated to daily bars (within 10 %); one bar per
trading day / seven per session under market hours; events landing in the bar
that contains them; regime clustering appearing only when the variance
half-life exceeds the bar; 30 years of daily bars in ~11 k steps.

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
| coarse mode | see §8.5: bar invariants for every interval, golden hash, statistical match with the fine path and the analytic BM range, market-hours bar counts, partial first bucket, event placement, GARCH scaling, mixing with fine steps. |
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
12. Coarse mode (§8.5) added after the first review. While adding it, the
    volume return of a tick was changed to include event jumps applied in
    that tick (a news jump now prints volume); `STATE_VERSION` is 2.
13. Trading (§14) is a liquidity layer around the unchanged price process,
    not an order-driven price: game events keep their promised size, the
    tape still shows them as flow, and traders move the price through a
    square-root impact that reverts with the spread.

---

## 14. Trading layer

Added after the price simulator was in use. The request: let players open
orders, match them, have their trades influence the price, and populate the
market with synthetic orders so it looks alive; and decide how all of that
relates to the price simulation.

### 14.1 How trading relates to the price simulation

Three architectures were considered.

**A. Order-driven price.** Retire the latent process; the price is whatever
the last trade printed, and *everything* (players, game events, background
noise) is an order. Game events become synthetic order flow, e.g. a scandal
is a wave of market sells. Rejected:

- The size of an event's move becomes a function of book depth at that
  moment, so "scandal = −10 %" is no longer a promise the game can make; it
  has to be tuned per symbol and re-tuned whenever liquidity changes.
- Mean reversion, GARCH clustering, jumps and overnight gaps have no natural
  home; they would all have to be re-invented as agent behaviour, which is
  more code, more parameters and much harder to validate statistically than
  the current tests (§11).
- Coarse mode (years of daily history in thousands of steps) is impossible:
  an agent-based market has to be simulated tick by tick.
- Cost: every tick becomes many order-book operations instead of ~7 libm
  calls.

**B. Reference price plus liquidity layer** (chosen). The simulator remains
the *reference price* `R_t`: the fundamental, the reverting spread, GARCH,
jumps, game events and coarse history all stay exactly as they are. The
order book is a liquidity layer around it:

- Synthetic **makers** quote a ladder of limit orders around `R_t`, re-quoted
  every tick (§14.3). Their quotes are what a player's market order hits.
- Synthetic **takers** realise the simulator's tick volume as prints against
  the book (§14.4), with a direction biased by the tick's return. A game
  event therefore *does* show up on the tape as a burst of one-sided flow,
  as the user suggested, but the flow is a rendering of the move the
  simulator already made rather than its cause. The move's size is still the
  event's.
- **Traders** (players, or NPCs the game runs under a `TraderId`) send orders
  that match against both. Their net flow feeds back into the simulator as
  price impact (§14.5), so trading moves the price, and the move decays
  through the same mean reversion as every other shock.

Invariant: **with no trader orders the exchange's ticks are bit-identical to
the bare simulator's** (price *and* volume). The synthetic flow draws from a
separate RNG stream (`long_jump` of the seeded generator), so adding the
book cannot perturb the reference path, and existing golden hashes still
hold. `tests/trading.rs` pins this.

**C. Simulate the past, order-drive the present** (the option raised in the
request). History from the simulator, live trading purely order-driven.
This inherits every drawback of A for the live part and adds a seam: the
statistical character of the series changes at the moment the game starts.
B gives the same visible outcome (events manifest as flow on the tape,
players see their orders in a book) without the seam, and the game can still
choose to express an event as an NPC trader's orders when it wants the move
to *emerge* from matching rather than be dictated.

So a game has two ways to act on the market, and can mix them:

| route | what happens | move size |
|---|---|---|
| `Simulator::push_event` (existing) | jump / drift / vol / fundamental on the reference; the tape shows the corresponding flow | exactly as specified, reverts per §4 |
| `Exchange::submit` under an NPC `TraderId` | orders match against the book and other traders; net flow moves the reference through impact | emerges from book depth and the impact law |

### 14.2 Order book (`book.rs`)

Plain price–time priority, `no_std`, deterministic:

- `BTreeMap<price, VecDeque<Resting>>` per side plus an id index; no hashing
  anywhere, so iteration order is fixed (§9 rule 4).
- `Order { owner, side, kind: Market | Limit { price_cents }, tif: Gtc | Ioc | Fok, qty }`.
  The taker pays the maker's price. `Placement` reports fills, remainder and
  where it went (`Filled | Resting | Cancelled`).
- `Trade { ts, price_cents, qty, taker_side, taker: Party, maker: Party }`;
  `Party { order: OrderId, owner: Synthetic | Trader(id) }`. `OrderId::HIDDEN`
  (zero) marks hidden liquidity (§14.4).
- `preview(side, qty, limit)` walks the book without mutating it, so a game
  can check affordability before submitting.
- Self-trades are allowed (a trader's buy may hit their own ask); they net to
  zero flow.

### 14.3 Synthetic makers

Every tick, after the reference moves, all synthetic quotes are cancelled and
re-placed (`LiquidityParams`):

```
regime  = max(1, σ_t / σ)                          spread widens in panic
hs      = half_spread · regime
bid_1   = min(R − 1, ⌊R (1 − hs)⌋)                  never inside one cent
ask_1   = max(R + 1, ⌈R (1 + hs)⌉)
step    = max(1, round(R · level_step))
bid_k   = bid_1 − k·step,  ask_k = ask_1 + k·step,  k = 0 … levels−1
size_k  = touch_depth · base_per_day · depth_growth^k · exp(η z − η²/2)
```

Defaults: 5 bp half spread, 10 levels 5 bp apart, 0.1 % of a day's base
volume at the touch growing 1.3× per level (≈ 42 k shares per side for the
default config), lognormal size noise 0.3. Re-quoting is one normal draw per
level and side from the flow RNG.

A new quote that crosses a trader's resting order executes against it at the
trader's price: a bid wall below the market gets eaten level by level as the
reference falls through it, exactly as a resting limit should. Those fills
count as trader flow (§14.5), so a wall does hold the price up — at the cost
of the shares it absorbs.

Market orders from traders are converted to immediate-or-cancel limits
`market_collar` (default 5 %) from the reference, so a fat-fingered order
cannot sweep a stray trader ask at ten times the price.

### 14.4 Synthetic takers

The simulator's `Tick::volume` is spent as prints against the book
(`FlowParams`), before the ladder is re-quoted, so the prints walk the
*previous* ladder toward the new price and the tape reads "buyers swept the
asks, market re-quoted higher":

```
r       = ln(R_new / R_old)
P(buy)  = ½ + ½ · imbalance · tanh(r / σ_tick)      default imbalance 0.8
n       = 1 + Poisson(max(1, V / mean_print) − 1), capped at max_prints
weights w_i ~ U[0.5, 1.5);  print_i = V · w_i / Σw  (last print takes the remainder)
```

Each print is a synthetic market order. Whatever the visible book cannot
absorb prints against **hidden liquidity** at the new reference price
(`maker.order = OrderId::HIDDEN`), so the tape volume always equals the
simulator's volume and no share of a game event's volume spike is lost.
Synthetic prints also fill traders' resting orders that stand at or inside
the touch, which is how a limit order inside the spread gets filled without
the reference ever crossing its price.

Draw order per tick (flow RNG): one uniform for the print count, one per
print for its weight, one per print for its side; then the ladder's normals.
Frozen by `EXCHANGE_VERSION`.

`StepReport.tick.volume` is the tape volume of the step: synthetic prints
plus every trader execution since the previous tick (orders submitted
between ticks are added to the next tick).

### 14.5 Price impact

Net signed shares that traders took from synthetic liquidity since the last
tick, `Q` (trader-to-trader trades net to zero, and a trader's passive fills
count the same as aggressive ones: the rest of the market had to be induced
to supply those shares), become an event on the simulator at the next tick
(`ImpactParams`):

```
Δ ln P = sign(Q) · min(1, coefficient · σ_day · (|Q| / ADV)^exponent)
σ_day  = σ / sqrt(trading days per year)
ADV    = base_per_day · (1 + return_sensitivity · sqrt(2/π))     (§4.7)
```

The square-root law (`coefficient` ≈ 0.7, `exponent` 0.5) is the standard
empirical shape. The move is pushed as `Jump(e^{Δ} − 1)` on the spread, so
it is **transient**: it decays with the spread half-life `ln2/θ` like any
other shock, which is also the empirically right shape for impact. An
optional `permanent_fraction` routes part of it to `FundamentalShift`
instead. Because the OU spread is linear and the simulator's RNG is
untouched, the treated path differs from a seed-matched control by exactly
`Δ·e^{−θt}`; `tests/trading.rs` checks this to 1e-7.

Consequences a game gets for free: buying then selling loses the spread
plus the impact you paid on the way in; a pump reverts unless the fundamental
follows; a big wall supports the price only while it lasts.

### 14.6 API and state

```rust
pub struct Exchange { .. }   // serde(try_from = "ExchangeRepr") with its own EXCHANGE_VERSION
impl Exchange {
    pub fn new(config: Config, params: TradingParams, seed: u64) -> Result<Self, ConfigError>;
    pub fn step(&mut self) -> StepReport;                                    // one tick
    pub fn advance(&mut self, dur: Duration) -> impl Iterator<Item = StepReport> + '_;
    pub fn submit(&mut self, order: Order) -> Result<Placement, OrderError>;  // matches now
    pub fn cancel(&mut self, id: OrderId, trader: TraderId) -> Result<Resting, CancelError>;
    pub fn cancel_all(&mut self, trader: TraderId) -> Vec<Resting>;
    pub fn preview_market(&self, side: Side, qty: u64) -> Preview;
    pub fn book(&self) -> &OrderBook;
    pub fn simulator(&self) -> &Simulator;
    pub fn simulator_mut(&mut self) -> &mut Simulator;   // events, coarse history
    pub fn reference_cents(&self) -> i64;
    pub fn pending_flow(&self) -> i64;
}
pub struct StepReport { pub tick: Tick, pub trades: Vec<Trade>, pub impact: f64 }
pub struct Position { qty, cash_cents, cost_cents, realised_pnl_cents }   // average-cost ledger helper
```

`Exchange` wraps a `Simulator`; the game generates history through
`simulator_mut()` (coarse bars, or fine ticks, which with no traders are
the ticks the exchange would have produced), calls `resync()` to quote the
ladder, and then steps the exchange. Serialisation nests `SimulatorRepr`
inside `ExchangeRepr`, so save games carry the book, the flow RNG and the
pending flow. Cost: re-quoting is ~40 `BTreeMap` operations per tick, so
`bench ticks/exchange` runs at ≈ 150 k ticks/s against ≈ 5 M for the bare
simulator (hence the warm-up shortcut above). If that ever matters, the
ladder could live in a fixed array merged into matching instead of the map.

Users, accounts (cash, reservations for resting orders, no shorting) and the
cash ledger live in the web app (`webapp/src/account.rs` and
`webapp/src/trading.rs`), not the crate: they are game rules. `Position` is
in the crate because every consumer needs the same average-cost arithmetic.

### 14.7 Web app

Per symbol the `SymbolState` now owns an `Exchange` and a bounded tape.
New endpoints: `POST /api/traders`, `GET /api/traders[/{id}]`,
`POST /api/traders/{id}/cancel_all`, `POST /api/traders/{id}/deposit`,
`POST|GET /api/users`, `GET /api/users/{id}`,
`POST|GET /api/users/{id}/accounts`, `GET /api/accounts[/{id}]`,
`POST /api/accounts/{id}/deposit|withdraw|status`,
`GET /api/accounts/{id}/ledger|validate`, `POST|GET /api/symbols/{s}/orders`,
`GET|DELETE /api/symbols/{s}/orders/{id}`, `GET /api/symbols/{s}/book`,
`GET /api/symbols/{s}/trades`, `GET /api/symbols/{s}/shares` and
`GET /api/users/{id}/holdings`. The `tick` stream message carries the best
bid/ask, the top of the book and the step's prints; `fill` messages report
a trader's executions. The UI gains a book ladder, an order ticket, the
account and open orders, a tape, and fill markers on the chart. The four
seeded symbols differ in liquidity: NBLA is thin and wide, PXCO deep.

### 14.8 Users, accounts and money

Money is modelled in three pieces, in `webapp/src/account.rs`:

* A **user** is the person: a name, an optional email, and the accounts
  opened for them.
* An **account** holds the money: a balance, the part of it reserved for
  resting buy orders, a status (`active` / `frozen` / `closed`), and a
  bounded ledger — one entry per movement (`open`, `deposit`, `withdrawal`,
  `buy`, `sell`) carrying the signed amount and the balance after it.
* A **trader** (`webapp/src/trading.rs`) is the market-facing identity. It
  keeps positions, share reservations and fills, and trades on exactly one
  account; several traders may share one, and one user may have several.

Every amount is an `i64` count of cents. There is no floating point in the
money path, formatting included (`account::money` divides by 100 and prints
the remainder), and `notional_cents` does `price × qty` in `i128` before
clamping back, so a game-sized order cannot overflow a balance. Amounts must
be positive, a balance is capped at `MAX_BALANCE_CENTS` (10^15 cents) and
`Account::issues` lists any invariant that is nonetheless broken — a
negative balance, or more reserved than held — which
`GET /api/accounts/{id}/validate` reports.

An order is validated against its account *before* it reaches the exchange:
`Account::authorise` refuses it unless the account is active and its
available balance (`balance − reserved`) covers the worst case — `qty ×
price` for a limit, the ladder-walk preview for a market order. What rests
reserves cash on the account; what fills settles through it, so every cent
that moves is on the ledger and the sum of the entries is the balance.
Cancels release the reservation, and reserved cash can be neither withdrawn
nor closed out from under a resting order.

### 14.9 Shares: how many exist, and who may sell them

Shares are counted as strictly as cents, from both ends.

*Supply.* `SymbolInfo::shares_outstanding` fixes how many shares of a symbol
exist (240 M ACME, 85 M NBLA, 610 M HLIO, 150 M PXCO). `Market` splits that
number three ways — `held_shares` (the traders' positions), `bid_shares` (the
remainder of their resting buys, counted as spoken for) and
`available_shares` (the rest, what the synthetic book can still supply) — and
a buy for more than `available_shares` is refused with
`Refused::SupplyExhausted` before any cash is looked at. `GET
/api/symbols/{s}/shares` reports the split and the holders; the quote carries
`shares_outstanding` and the market cap it implies.

*Ownership.* A trader's shares live in its own `Position`, and `Trader::check`
refuses a sell whose quantity exceeds `free_shares` — the position less
whatever earlier resting sells already promised (`reserved_shares`). So there
is no shorting and no selling what a fill has not delivered, and shares belong
to the trader that bought them: one trader cannot sell another's, even under
the same user. A resting sell reserves shares exactly as a resting buy
reserves cash; a fill or a cancel releases them.

*Per user.* `Market::user_holdings` adds a user's positions up per symbol
across every trader of theirs — owned, reserved, still sellable, cost, mark
and P&L — behind `GET /api/users/{id}/holdings`, and `UserDto` carries the
totals (`shares_owned`, `holdings_value_cents`) next to the cash balance.

### 14.10 Orders: the log, and sending one twice

The book holds an order only while it rests: once it fills or is cancelled it
is gone, which is no basis for a client that wants to know what happened.
`Market` therefore keeps a bounded log (`FEHU_ORDER_LOG`, 2 000 by default) of
`OrderRecord`s — the submission as it was accepted, plus `filled`,
`remaining`, `status`, `notional_cents` and `updated_at_ms` as they move.
`Market::apply_trades` is the one choke point where fills are booked, so a
resting order's record follows it whether it is hit by another trader's order
or by the engine's synthetic flow; cancels mark it from the two cancel paths.
Eviction drops the oldest *finished* records first and never a live one.
Before a trader submission, the target book's id counter advances to the
largest next id across all symbol books under the market lock. This keeps
trader order ids unique across symbols, including after synthetic flow and
restarts, without changing prices or resting orders. Fill and cancel updates
also require a matching symbol. Older builds could overwrite records when
separate books assigned the same id; already lost history cannot be recovered.

Three checks stand between a submission and the book, all in `api.rs` because
they are exchange rules rather than book mechanics. **Post-only**
(`"post_only": true`) refuses a limit order whose price is already tradable —
`best_ask ≤ price` for a buy — so a maker never becomes a taker by accident; it
is meaningless for a market order or a non-`gtc` one, and both are refused as
invalid. **Self-trade prevention** refuses an order that would reach one of the
same trader's resting orders: `self_crossing` asks the book's own preview how
far down the other side the order would walk and flags only the trader's orders
inside that reach, so a resting bid far from the market does not block a market
sell. A self-trade moves neither shares nor money, but it prints on the tape and
drags the reference with it, which is exactly what an exchange stops. **Amend**
(`PATCH /api/symbols/{s}/orders/{id}`) is a cancel and a fresh order under one
lock — the crate's book has no in-place amend, and a price change would lose
priority anyway — so the replacement goes to the back of its price queue, and
the response names the withdrawn order and how much of it had filled. Both
handlers place the order through one `place_order`, which is where the cash,
share-supply and ownership checks, the fills, the reservation and the order log
all live.

A submission may carry a `client_order_id` (≤ 64 characters, unique per
trader), which makes it idempotent: a repeat with the same parameters is not
sent to the book at all — the response the first one produced is stored on the
record and replayed verbatim with `200` instead of `201` — and a repeat that
asks for something else is refused with `409 duplicate_client_order_id`. That
is what makes a retry after a timeout safe, which no amount of care on the
client side can otherwise guarantee.

### 14.11 Who a request speaks for

Market data is public; everything a user owns is not. Each user is issued one
API key when they are created (`webapp/src/auth.rs`: 128 bits of
operating-system entropy behind a `fehu_` prefix), returned **once** — in the
response that created them. `Keyring` retains a domain-separated SHA-256
hash for authentication and persistence. A newly issued credential can be
taken once for that response; restored keyrings cannot recover it. Debug
output omits both keys and hashes. These are random 128-bit credentials,
not user-chosen passwords.

A request presents it as `Authorization: Bearer <key>` or `X-Api-Key`, and the
`Caller` extractor turns that into a `UserId` — 401 with no key or an unknown
one. Handlers then check ownership rather than trusting the ids in the path:
`owned_trader`, `owned_account` and `owned_user` refuse somebody else's
property with 403, listings (`/api/traders`, `/api/users`, `/api/accounts`)
return only the caller's own, and the holder list on
`GET /api/symbols/{s}/shares` names only the caller's traders — the totals are
public, the names behind them are not. Sign-up itself (`POST /api/users`, and
`POST /api/traders` with no `user_id`) is the one unauthenticated write;
joining an existing user needs that user's key.

Two places take the key differently. The SSE stream carries fills, which
belong to the trader that made them, but `EventSource` cannot set headers, so
`/api/stream?api_key=` filters them: a stream with no key, or an unknown one,
gets ticks and events only. And the game-master endpoints, which push events
that move prices, are gated by `FEHU_ADMIN_KEY` when it is set — compared in
constant time — and open when it is not, which is what a single-player game on
localhost wants and a shared server does not.

### 14.12 Sessions and halts

Two things stop trading, and they are not the same thing.

*Sessions.* `FEHU_MARKET_HOURS=09:30-16:00` gives every symbol the crate's
`MarketHours` calendar (UTC, Monday to Friday), which the simulator already
understands: ticks exist only inside sessions and each gap carries its
overnight move (§3.3). The web app adds the trading half — outside a session
`submit_order` refuses with `409 market_closed` — and reports `market_open`,
`next_open_ms` and `next_close_ms`. Unset, there is no calendar and the market
never closes, which is what a game whose players log in at all hours wants.

*Halts.* Each symbol carries a band, `band_cents`, set where its day opened —
re-measured whenever a daily bar closes — and a price more than
`FEHU_PRICE_LIMIT_PCT` away from it halts the symbol for `FEHU_HALT_SECS` of
simulated time. `Market::review_halt` runs at the end of every engine step,
which is also where an automatic halt lifts itself; the band is then measured
again from wherever the price got to, so a resume cannot immediately re-halt
on the same move. The game master can halt a symbol by hand
(`POST /api/symbols/{s}/halt`), and a manual halt has no `until_ms`: only a
resume lifts it. `FEHU_PRICE_LIMIT_PCT=0` turns automatic halts off.

A halt stops *orders*, not the world: the simulator keeps generating the
reference price, so a symbol that reopens has moved, the way a real one gaps
on the news that halted it. Resting orders stay resting and their reservations
stay held — and cancelling is always allowed, whether the market is closed,
halted or both, because a player must be able to pull an order out of a market
that has stopped.

Staying resting means staying *unfilled*. A halted symbol advances through
`Exchange::advance_without_matching`, which moves the reference process but
runs neither the synthetic flow nor a requote, so the book — quotes, resting
orders and their queue positions — is exactly what it was when trading
stopped, however far the price has travelled meanwhile. Resuming (by hand or
when an automatic halt's time is up) calls `Exchange::resync`, which requotes
around wherever the reference got to; whatever those new quotes cross settles
through `Market::apply_trades` as ordinary maker fills and reaches their
owners as `fill` stream messages. So the gap is priced into one reopening
print rather than into a trickle of fills nobody could have cancelled. `GET /api/symbols/{s}/status` answers all of this in one
place, quotes carry `market_open` and `halted` for the symbol rail, and a
`status` stream message reports every change.

### 14.13 Stops: orders the book has not heard of yet

A stop is not an order. It rests nowhere, holds no queue position, reserves
neither cash nor shares, and the book has never been told about it. It is a
line drawn on the price, and `SymbolState::stops` is where the lines for one
symbol are kept — a plain `Vec<StopOrder>`, oldest first, saved with the
symbol so a restart does not lose anybody's exit.

`POST /api/symbols/{s}/stops` arms one: a side, a quantity,
`stop_price_cents`, and optionally `limit_price_cents`, which is what makes
it a stop-limit rather than a plain stop. The trigger must be on the far side
of the market — a buy above the last price, a sell below — because a stop
that has already been reached is a market order wearing a disguise, and
almost always a typo. Unlike an order it is accepted while the symbol is
halted or its session closed: the trigger simply waits.

`Market::fire_stops` runs at the end of every engine step, after
`review_halt` so a symbol that resumed in this step fires the triggers the
price reached while it was stopped, and skipped entirely for a symbol that is
halted or outside its session. A buy fires at or above its price, a sell at
or below, both against the last tick, oldest stop first. Firing removes the
stop and sends the order it became through `Market::place` — the same door as
any submission, so it takes the same cash, share and supply checks, prints
the same fills, and is written to the same order log.

That second check is the point of the design rather than an afterthought. A
stop reserves nothing while it waits, so the money that would have paid for
it can be spent or withdrawn in the meantime; the check when it fires is what
catches that. A stop that fires and cannot be placed is spent either way: it
is reported, not retried, because a trigger that keeps trying is a different
instrument. Both outcomes reach the trader as a `stop_triggered` stream
message carrying the trigger, the price that reached it, and either the order
or the refusal — private to its owner, like a fill, because a stop says what
somebody intends to do.

Stops are listed per symbol (`GET /api/symbols/{s}/stops?trader_id=`), per
trader (`GET /api/traders/{id}/stops`) and in the portfolio, withdrawn with
`DELETE /api/symbols/{s}/stops/{stop_id}`, and capped at
`MAX_STOPS_PER_TRADER` per trader — a trigger costs its owner nothing to
hold, which without a cap is somebody else's memory. Stop ids are their own
sequence: a stop only enters the order log when it becomes an order. The
store holds untriggered stops only, so a fired one lives on as its order
record and its stream message, not as a stop with a terminal status.

### 14.14 Keeping the market across a restart

Prices are reproducible from a seed; accounts are not. `webapp/src/save.rs`
therefore writes the whole `Market` to one file (`FEHU_STATE_FILE`) every
`FEHU_SAVE_SECS` and once more on a clean shutdown, and `main` reads it back
at start-up in place of the warm-up.

The file is versioned JSON (`STATE_VERSION`), with the crate's own `Exchange`
and `Candles` representations nested inside the web app's users, accounts,
traders, order log, event log and keyring — `serde_json`'s `float_roundtrip`
is on, so a restored simulator continues bit-exactly. Symbols are the one
thing not saved: their metadata comes from the build and only the ticker is
written, so a file listing symbols this build does not have is refused, as is
one from an unsupported format version. Version 2 is migrated to version 3
by hashing the saved API keys; the next write contains only hashes, while
existing backups remain unchanged. Refusing unknown formats is the point — a
market that comes back without its accounts is worse than one that does not
come back.

Two details make the restart seamless. `sim_now_ms` is the furthest the
market reached — the clock, or a symbol's own clock if an engine step left it
ahead — and start-up continues from there, so nothing is frozen waiting for
wall time to catch up. And the response each order was accepted with is saved
beside the order log, so a `client_order_id` retried across a restart still
replays rather than being refused.

Writes are atomic: buffered output is explicitly flushed with errors checked,
then synced to disk. The snapshot goes to `<file>.tmp` and is renamed over the
target, so an interrupted write leaves the previous save intact. A failed
periodic save is logged and retried at the next tick; it never takes the
server down.

Tickers are `&'static str` throughout the server (`save::Symbol`), which
`serde` would otherwise treat as data borrowed from the input; the alias hides
that from the derive, and the `symbol*` modules turn a ticker on disk back
into one of the build's four, refusing anything else.

### 14.15 Tests

`tests/trading.rs`: book priority/partial fills/IOC/FOK/cancel/preview; the
no-trader invariant against the bare simulator for 20 k ticks; ladder
geometry and spread widening under a vol shift; market buy pays the spread
and moves the reference by the analytic amount; exact impact decay and
round-trip cost against a seed-matched control; permanent fraction lands on
the fundamental; resting bids/asks fill when the market trades through them;
trader-to-trader trades leave the reference alone; ownership on cancel;
market-order collar; flow direction follows the return (> 70 % of volume);
determinism with interleaved orders; serde round trip (JSON and postcard)
continuing identically for 2 k ticks. `webapp/tests/api.rs` covers the HTTP
surface end to end, including reservations and rejections; users opening
accounts and paying money in, the ledger adding up to the balance after a
fill, refused amounts (zero, negative, over the cap, fractional JSON,
overdrawn), a frozen account refusing orders and withdrawals while still
taking deposits, and one user running several traders. Shares: a sell is
refused with nothing owned, above what is owned, and above what resting sells
leave free, and goes through again once they are cancelled; the per-symbol
count splits into held, bid for and available, a buy beyond it is refused
before the cash check, and a user's holdings add up across their traders
while one trader still cannot sell another's shares. Orders: a
`client_order_id` replays the first response and refuses a mismatched reuse
while staying per-trader, a filled or cancelled order is still readable from
the log once the book has dropped it, the history filters by status, and a
resting order's record follows its partial fills to `filled`; a post-only order
that would cross is refused while one that rests is taken, a trader cannot
reach its own resting order but another trader can, and an amendment replaces a
partly-filled order with one for the remainder, releasing what the old one
reserved. Keys: every
private endpoint answers 401 without one and 403 with somebody else's, having
moved nothing; a forged key is refused; listings show the caller their own
only; market data stays open; and `FEHU_ADMIN_KEY` locks the game-master
endpoints while leaving the event log readable. `webapp/tests/save.rs`: a
saved market comes back whole — the same portfolio, ledger, order log, book,
bars, events and holders, with keys that still work, ids that carry on and a
`client_order_id` that still replays — a new player can still sign up
afterwards, a file from another version or with unknown symbols is refused,
and a failed write leaves the previous save intact, a halt included. Sessions
and halts: a limit move halts a symbol and refuses its orders while the others
carry on, the halt lifts itself and re-bands, a manual halt outlasts any amount
of time and only the game master can place or lift one, a resting order
survives a halt and can still be cancelled, a halted book fills nothing while
the price moves through it and the resume settles what the new quotes cross,
and a closed session refuses orders while an open one takes them. Stops: a
trigger waits, fires when the price reaches it, becomes an order in the log
and tells its owner; one behind the market, unfunded or malformed is refused
when it is armed; it is private property that only its owner may see or
withdraw; a halted symbol holds its triggers and fires them on the resume;
one whose money has left in the meantime is refused when it fires rather than
half-placed; and held stops come back from a save and still fire.
The stream: a client that falls behind is disconnected rather than skipped,
a reconnect with `?since=` replays exactly what it missed once each and in
order, asking for more than the buffer holds is answered with `gap: true`
alongside what is left, and a replay hands nobody somebody else's fills.
`webapp/tests/contract.rs` pins the JSON key sets the TypeScript UI is typed
against.

---

## 15. Not built yet

What follows is the work the trading layer still wants, in the order I would
do it, and the decisions already taken that a real venue would revisit. It is
here rather than in an issue tracker because each item is a design question
first and a patch second.

### 15.1 Orders the book will not take

**Stop and stop-limit (implemented, §14.13).** Held per symbol, fired at the
end of every engine step against the last tick, placed through
`Market::place` and logged like any other order. The funds are checked twice
— once when the stop is accepted and once when it fires — because a stop
reserves nothing while it waits. A halted or closed symbol holds its
triggers and fires them on the resume, `stop_triggered` reports both
outcomes to the owner, and save format 4 carries the untriggered stops.

**Iceberg, GTD and day orders.** Iceberg needs the book to re-post a slice as
each one fills, which is `book.rs`, not the web app. GTD and day orders need
an expiry sweep on the engine step, which is easy but pointless until sessions
are the default rather than an option (§14.12).

**Tick and lot size.** Prices are integer cents and quantities whole shares,
and nothing else is enforced: a symbol cannot say "quote me in five-cent
steps" or "trade me in lots of ten". `TradingParams` is where they would go,
with the check in `OrderBook::validate` so the crate refuses them rather than
the web app.

### 15.2 What the stream promises (implemented)

Every message the server publishes goes through `App::publish`, which gives
it the next sequence number and keeps it in a bounded ring buffer
(`FEHU_STREAM_REPLAY`, 1024 by default). The number rides in the envelope
alongside the tag — `{"seq": 41, "type": "tick", …}` — so a client can tell a
quiet market from a gap without guessing.

`GET /api/stream?since=N` resumes: everything published after `N` that the
buffer still holds is replayed, oldest first, before the live feed. The
subscription is taken while the sequence lock is held, so no message can slip
between the replay and the live feed — each arrives exactly once, in order.
The `hello` that opens a connection carries `seq` (where the connection
joins, so the first live message is `seq + 1`), `oldest_seq` (how far back
`?since=` can still reach) and `gap`, which is set when the client asked for
more than the buffer had. `gap` is the honest answer to the one question the
protocol could not previously answer: *did I miss anything?* Only then does a
client have to reload its snapshots.

Privacy survives the replay. A fill and a `stop_triggered` belong to their
trader, and the buffer holds everybody's, so the same filter runs over the
replay as over the live feed: a stream that has not proved who it speaks for
gets neither, however far back it asks.

Falling behind still ends the connection rather than silently skipping —
`BroadcastStream`'s lag error terminates the stream — but now the client
reconnects with the sequence it got to and picks up where it left off. The
bundled UI does exactly that: it reopens at its own `seq` rather than letting
`EventSource` rejoin the live feed, and reloads snapshots only when the
`hello` reports a gap.

What is still not promised: the buffer is memory, not a log, so a client that
is away longer than `FEHU_STREAM_REPLAY` messages gets `gap: true` and has to
resynchronise; nothing is persisted, so a server restart starts the sequence
at 1 again and every `?since=` is a gap.

### 15.3 Money the market does not move

**Fees.** Nothing is charged: no commission, no maker rebate, no exchange fee.
They belong on the settlement path (`Trader::book_fill` → `Account::settle`)
as a separate ledger entry per fill rather than as an adjustment to the price,
so the tape stays the price and the ledger stays the money.

**Corporate actions.** `shares_outstanding` never changes, so a split, a
dividend and a buyback that retires stock are all unrepresentable — the
`buyback` game event moves the price and nothing else. A split is the
awkward one: it rewrites every position's quantity and average cost, every
resting order's price and size, and the bar history, or the chart lies.
Dividends are easier and would land as a ledger entry against holders of
record.

**Listing and delisting.** The symbol set is fixed at build time
(`TICKERS`), which the save format depends on: a file listing other symbols is
refused (§14.14). Adding symbols at runtime means a symbol table in the save
file and a story for what happens to a delisted symbol's positions.

### 15.4 Running it for more than a game

* **Reconciliation (implemented).** `GET /api/reconcile` runs under the
  market lock and uses the same game-master authentication as event writes.
  It checks account/user/trader links, account invariants, cash reservations
  against all resting buys (including shared accounts), share reservations
  against resting sells, nonnegative positions and the outstanding-supply
  bound. It checks consecutive ids and arithmetic in each retained ledger
  and reconciles its closing balance to the account. Evicted ledger history
  cannot be verified: the first retained entry supplies the opening anchor.
  Synthetic quotes are replenished liquidity, not an independently tracked
  inventory, so the supply check is holdings plus resting bids ≤ outstanding.
  The report is read-only and does not repair inconsistent state.
* **Rate limits.** There are none. One client can submit orders as fast as it
  can open sockets.
* **One lock.** `Mutex<Market>` serialises every order across all four
  symbols. Splitting it per symbol — with the accounts still shared — is the
  first scaling step, and the point at which the ordering guarantees now
  provided by "there is one lock" have to be written down.
* **Metrics.** `/api/health` counts ticks, trades and users. There is nothing
  on latency, order rates, or how long the engine step takes.

### 15.5 Decisions a real venue would revisit

These are deliberate, and each is a place where the game and an exchange part
company.

* **The game-master endpoints are open unless `FEHU_ADMIN_KEY` is set.** That
  keeps the bundled UI's event panel working out of the box; on a shared
  server it means anyone can move the prices until the variable is set.
* **A halt stops orders, not the world** (§14.12). The book is frozen and
  nothing matches, but the simulator keeps moving the reference, so a symbol
  reopens gapped and the reopening print is a plain requote rather than the
  auction a real venue would run.
* **An amendment is a cancel and a fresh order** (§14.10). It loses queue
  position, which a real in-place amend of quantity-down would not, and if the
  replacement cannot be placed — no cash, no shares, a halt in between — the
  original is already gone.
* **The stream takes its key in the query string**, because `EventSource`
  cannot set headers. Keys can therefore reach access logs.
* **Restore validation covers accounting and identities.** The file reader
  checks nonzero unique user/account/trader/event ids and counters beyond
  retained ids, one well-formed key digest per user, then runs market-wide
  reconciliation before accepting a snapshot. Inconsistent balances, retained
  ledgers, ownership, reservations and share supply are refused. This is not
  a complete validator for the simulator or historical order records; those
  remain further work. Book validation checks nonempty price levels, valid
  quantities, uncrossed sides, FIFO id ordering, unique ids below the next
  counter and an exact match between the index and resting orders. The
  low-level `App::restore` constructor expects an already validated snapshot
  from `save::read`.

