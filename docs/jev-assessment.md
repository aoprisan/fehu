# Can fehu use Jev (TypeSafe AI)?

An assessment, taken at the head that finished the second missing-features
reading, of whether TypeSafe AI's **Jev** model has a place in this
repository, and where.

**Short answer: it can be used beside the server, not inside it, and
nothing measured asks for it yet.** The one honest fit is on the game
backend's side of the HTTP surface — turning a free-form game happening into
the `GameEventRequest` the server already accepts — and that code belongs to
the game backend, not to `fehu-economy`. Nothing in this workspace should take
a dependency on it today.

## What Jev is

Jev is the first "System One" model from TypeSafe AI, launched in September
2026. It is not a text generator. A request carries a block of application
*state* and a map of typed *questions*; the response is a typed answer per
question, with a probability distribution and a confidence, that code can
branch on without parsing. Three question kinds exist:

| Kind | Asks | Returns |
|---|---|---|
| `noul` | does a condition hold | probability of yes |
| `choice` | which of a named set applies (up to 255 options) | the pick, a probability per option, a confidence |
| `score` | where on 2–10 ordered, described levels | a weighted position, a probability per level, a confidence |

The endpoint is `POST https://api.typesafe.ai/v1/systemone`, bearer-keyed.
State is JSON: plain text, or text plus named facts. Questions in one request
are evaluated independently and in parallel; adding questions adds little
latency, and sequential dependencies need separate calls. TypeSafe's own demo
answers in about a tenth of a second. Pricing is per input token only
($0.042 per million, output free), so cost is not a consideration at any
volume this server produces.

What is documented about it, and what is not, matters for the verdict:

- **Closed, managed API.** No weights, no self-hosting, no on-premises
  deployment. Keys are issued from a console to a waitlist, in batches, as of
  this writing.
- **No determinism guarantee.** Nothing published says the same state and
  questions return the same probabilities twice, and there is no seed
  parameter. Treat every answer as a reading of an external, non-reproducible
  source.
- **Bounded decisions only.** Its own reference material says it is for
  semantic judgements over a fixed option set, not multi-step reasoning or
  anything open-ended, and that calibration is not correctness.
- **SDKs.** Official TypeScript (`@typesafe-ai/sdk`, Node 20+) and Python.
  No official Rust client; crates.io has several unofficial ones (`jev`,
  `typesafe-sdk`, `typesafe-client`, `typesafe-ai-rs`, `kunobi-jev`), every
  one a thin `reqwest` wrapper.

Some of the above comes from secondary sources: TypeSafe's documentation
site was not reachable from the environment this was written in, so the wire
shape was read from the JavaScript SDK's README and the `jev` crate's source,
and the pricing, latency and availability from the launch coverage.

## The constraints this repository puts on any external model

Three rules in `CLAUDE.md` decide the question before any use case does.

1. **`crates/fehu` is `no_std`, has no I/O, and is bit-reproducible.** A
   network call has no place in the library at all. Anything Jev decides
   would arrive as an *input* to the library — an event, a config, an order —
   through the same API a game uses.
2. **A command may not read anything a replay cannot.** `journal::apply` is
   the only implementation of every mutation, and the live path and the
   replay path are the same code over the same entry. An apply arm that
   called Jev would answer differently on replay, and a snapshot plus the
   journal would no longer reproduce the world. The pattern that exists for
   this — the API-key digest, the listing's seed, the step's instant — is
   that the *handler* resolves what replay cannot and journals the resolved
   value. A Jev answer would have to be journaled the same way: as the
   decision it produced, never as the question.
3. **The engine step is one market job with nothing waiting on it.** Every
   book is quiet while a step is in flight. A remote call inside it, even at
   100 ms, would stall every symbol on every tick, and a failed one would
   have to fall back to something deterministic anyway — at which point the
   deterministic thing is the policy.

So the only architecturally valid placement is: a handler (or the game
backend) asks Jev, turns the answer into an existing `Command`, and the
command carries the answer. Nothing else is on the table.

## Where it could attach, weighed

Scored the way `missing-features-analysis.md` scores everything: usefulness
against the acceptance scenario, cost to land it to this repository's
standard.

| Placement | Use | Cost | Reading |
|---|---|---|---|
| Inside `crates/fehu` | — | — | Ruled out by `no_std` and determinism. Not a scoring question. |
| NPC quoting inside `Command::Step` | 1 | XL | The policy is five integer knobs in basis points around a numeric reference; there is no text to judge and nothing semantic to decide. Making the step carry an external answer means the engine loop asks Jev before every step, journals the decisions as fields of `Step`, bumps the save format, and defines a fallback for a slow or failed call. All of that to replace arithmetic the policy already does deterministically and for free. Not until a game measures a merchant behaving wrongly for a reason a number cannot express. |
| A game-event classifier in `api.rs` (free text → `GameEventKind` + magnitude) | 2 | M | The one real fit. `GameEventKind` is a `choice` over twelve named options with a `description()` for each already written; magnitude is a `score` over a few described levels; `Scope` follows from the kind. The handler would resolve the answer and journal the ordinary `GameEventRequest`, with the `note` field carrying provenance. It fits the "resolve, then journal" rule exactly. But the requester is the game backend, which holds the `events` scope, has the text, and can call Jev itself and post the typed request — so the server would gain an outbound HTTP client, an `FEHU_TYPESAFE_KEY`, a network failure mode inside an admission-gated handler, and a hundred transitive crates, to do what its only caller can already do. |
| The browser UI | — | — | Would put an API key in the page. Ruled out. |
| Reconcile, audit, health | 1 | — | Every question these answer is arithmetic over integers with an exact expected value. A probabilistic judge adds nothing to a sum that must be zero. |

The distinction the table keeps drawing is the useful one: **fehu has no
text.** Every input the server takes is already typed — cents, units, basis
points, a kind from a closed enum — and every decision it makes is a
computation over those. Jev's whole value is turning unstructured state into
typed decisions, and that translation happens one layer up, in the game,
where the unstructured state lives. Placing it there is not a compromise; it
is the boundary this design already drew when it made the game backend the
third principal.

## Recommendation

- **No dependency in this workspace.** Not in `fehu`, not in
  `fehu-economy`, not in the UI. Nothing in the acceptance scenario asks a
  semantic question, and the server currently carries no HTTP client at all.
- **The game backend may use it freely**, against the surface that exists:
  ask Jev which `GameEventKind` a happening is and how large, then
  `POST /api/game-events` with the typed request. The `note` field is where
  the reasoning can go. That needs no change here.
- **If a use ever appears inside the server**, the shape is fixed in advance
  by the journal rule: ask in the handler, journal the answer as fields of an
  existing or new `Command`, never ask in `apply`, never ask in `Step`. Any
  such change is at least M for the command and a save-format bump if the
  answer is persisted, and should share the bump the other pending items are
  already waiting on.
- **Two things must hold before even that is worth opening**: an API key
  outside the waitlist, and a published statement on reproducibility. Without
  the first it cannot be tried; without the second every answer has to be
  treated as a one-time reading, which is fine for a journaled command and
  disqualifying for anything else.

## Sources

- TypeSafe AI: <https://typesafe.ai/>, docs at <https://docs.typesafe.ai/>
  (unreachable from where this was written), console at
  <https://console.typesafe.ai/>
- Official SDKs: <https://github.com/typesafe-ai/typesafe-sdk-js>,
  <https://github.com/typesafe-ai/typesafe-sdk-python>
- Unofficial Rust client whose source was read for the wire shape:
  <https://crates.io/crates/jev>
- Community reference and curated lists: <https://gist.github.com/pjburnhill/adf8d28efcad9df037bfdece178ef965>,
  <https://github.com/AbdelStark/awesome-typesafe>
- Launch coverage: The Register, InfoWorld, heise, The Rundown AI, GIGAZINE
  (September 2026)
