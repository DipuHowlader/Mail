//! Human input emulation.
//!
//! A driven browser is only convincing if its **pointer and keyboard look like a hand**, not like a
//! script. Every synthetic event a naive driver sends carries a tell the behavioural collectors score:
//!
//! * a click that teleports the cursor onto the target and presses for ~1 ms,
//! * a `keydown`/`keyup` pair a few milliseconds apart with no flight time between keys,
//! * a key stream from a session in which the mouse never moved at all.
//!
//! This module ports the model Clearcote ships ([`motion.ts`](../../../node_modules/clearcote/dist/motion.js)
//! and `humanize.ts`) to Rust so the vendored engine gets the same treatment through CDP:
//!
//! * **Minimum-jerk submovements.** A reach is a primary ballistic submovement plus one or two
//!   corrective ones (Meyer's optimized-submovement model), each following `s(τ)=10τ³−15τ⁴+6τ⁵`, so
//!   the velocity profile peaks mid-path and decelerates into the target the way a hand does.
//! * **Fitts's law duration**, not duration proportional to distance.
//! * **Coloured noise** — an Ornstein–Uhlenbeck drift plus an 8–12 Hz physiological tremor and a
//!   correlated sub-pixel jitter, never flat white gaussian noise (whose flat spectrum is itself a tell).
//! * **A seeded persona** drawn from the identity seed: one motor signature (speed, tremor, overshoot,
//!   handedness, cadence) consistent within a session and unlinkable across seeds.
//! * **Human dwell times** — a mousedown held 60–150 ms, a key held 45–120 ms, inter-key flight,
//!   thinking pauses, and fast pairs that *roll over* (the next key goes down before the last comes up).
//! * **Fat-finger typos are deliberately not simulated** (see [`Humanizer::plan_typing`]): a mail
//!   sender must never corrupt an address, and a correction path that mis-measures the field is a
//!   far greater risk than the marginal realism it buys.
//!
//! The module is pure — it turns a start point, a target and a persona into a list of steps — so the
//! browser layer ([`super::chromium`]) owns the actual CDP dispatch and the off-protocol sleeps.

// Which lanes are dispatched where (see `device-emulation.md`, "Interaction realism"):
//
// * **Typing** — live: `ChromiumDriver::insert_text` plans [`Humanizer::plan_typing`] and dispatches
//   it over CDP `Input.dispatchKeyEvent`, with the verified atomic `Input.insertText`/DOM path kept
//   as the fallback for long text, recipient data, and any attempt that did not verify. Recipient
//   data is never typed key-by-key: a bare address arrives as one protocol insert (typing ~20
//   characters of dwell/flight pauses would cost seconds per message), and a comma-separated list
//   must stay atomic anyway because Gmail chips the field on a keyed comma.
// * **Warm-up** — live: [`Humanizer::take_warmup`] runs once before the first keyboard interaction
//   (`press_key`/`insert_text`), so a keystroke is never a session's first input event.
// * **Pointer presses** — reserved: the Gmail flow opens compose/send through the DOM because the
//   CDP mouse-*press* path destabilises some Gmail layouts. [`Humanizer::plan_click_box`]/[`plan_move_to`]
//   stay compiled for the driver's `click()` building block and future flows.

#![allow(dead_code)]

use std::f64::consts::PI;

/// Text longer than this is inserted atomically (`Input.insertText`) rather than typed key-by-key.
///
/// A real hand types long paragraphs at ~40–70 wpm; at that rate a 2 kB body would take minutes per
/// message and turn a bulk send into a standstill. Mirrors Clearcote's own split between a typed
/// field and a paste of a large payload.
pub const HUMAN_TYPE_MAX_CHARS: usize = 200;

/// `mulberry32` — the small, fast, well-distributed PRNG Clearcote seeds its motor persona from.
pub struct Rng {
    state: u32,
}

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    /// `mulberry32`: one `f64` in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b_79f5);
        let mut t = self.state;
        t = (t ^ (t >> 15)).wrapping_mul(1 | self.state);
        t = t.wrapping_add((t ^ (t >> 7)).wrapping_mul(61 | t)) ^ t;
        ((t ^ (t >> 14)) as f64) / 4_294_967_296.0
    }
}

/// FNV-1a 32-bit hash of an identity seed (the same hash the motion model uses).
pub fn hash_seed(seed: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in seed.as_bytes() {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Standard-normal sample (Box–Muller).
fn gauss(rng: &mut Rng) -> f64 {
    let mut u = 0.0;
    let mut v = 0.0;
    while u == 0.0 {
        u = rng.next_f64();
    }
    while v == 0.0 {
        v = rng.next_f64();
    }
    (-2.0 * u.ln()).sqrt() * (2.0 * PI * v).cos()
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Uniform `f64` in `[lo, hi)`.
fn rand_f64(rng: &mut Rng, lo: f64, hi: f64) -> f64 {
    lerp(lo, hi, rng.next_f64())
}

/// Uniform integer in `[lo, hi]` inclusive (matches `randInt`).
fn rand_int(rng: &mut Rng, lo: i64, hi: i64) -> i64 {
    lo + (rng.next_f64() * (hi - lo + 1) as f64) as i64
}

/// Minimum-jerk position fraction `s(τ)=10τ³−15τ⁴+6τ⁵`.
fn min_jerk(tau: f64) -> f64 {
    let t = tau.clamp(0.0, 1.0);
    t * t * t * (10.0 + t * (-15.0 + 6.0 * t))
}

/// Cubic bezier point at parameter `e`.
fn bez(p0: f64, c1: f64, c2: f64, p1: f64, e: f64) -> f64 {
    let m = 1.0 - e;
    m * m * m * p0 + 3.0 * m * m * e * c1 + 3.0 * m * e * e * c2 + e * e * e * p1
}

/// The salt that separates the rollover stream from the persona draw (keeps the draw order stable).
const ROLLOVER_SALT: u32 = 0x9e37_79b9;

/// One identity's stable motor signature.
#[derive(Clone, Debug)]
pub struct Persona {
    device_hz: f64,
    tremor_hz: f64,
    tremor_amp: f64,
    drift_amp: f64,
    drift_tau: f64,
    jitter: f64,
    fitts_a: f64,
    fitts_b: f64,
    primary_frac: f64,
    overshoot: f64,
    max_corrections: u32,
    approach_bias: f64,
    rollover: f64,
    click_hold_min_ms: f64,
    click_hold_max_ms: f64,
    key_dwell_min_ms: f64,
    key_dwell_max_ms: f64,
}

impl Persona {
    fn from_seed(seed_int: u32) -> Self {
        let mut r = Rng::new(seed_int);
        let rollover = lerp(0.12, 0.38, Rng::new(seed_int ^ ROLLOVER_SALT).next_f64());
        let max_corrections = if r.next_f64() < 0.15 {
            3
        } else if r.next_f64() < 0.6 {
            2
        } else {
            1
        };
        Self {
            device_hz: lerp(110.0, 155.0, r.next_f64()).round(),
            tremor_hz: lerp(8.0, 12.0, r.next_f64()),
            tremor_amp: lerp(0.12, 0.5, r.next_f64()),
            drift_amp: lerp(0.1, 0.4, r.next_f64()),
            drift_tau: lerp(60.0, 160.0, r.next_f64()),
            jitter: lerp(0.25, 0.7, r.next_f64()),
            fitts_a: lerp(90.0, 150.0, r.next_f64()),
            fitts_b: lerp(120.0, 190.0, r.next_f64()),
            primary_frac: lerp(0.84, 0.94, r.next_f64()),
            overshoot: lerp(0.03, 0.08, r.next_f64()),
            max_corrections,
            approach_bias: (r.next_f64() - 0.5) * 0.5,
            rollover,
            click_hold_min_ms: 60.0,
            click_hold_max_ms: 150.0,
            key_dwell_min_ms: 45.0,
            key_dwell_max_ms: 120.0,
        }
    }

    /// A human mousedown→mouseup hold (ms), right-skewed.
    fn click_hold(&self, rng: &mut Rng) -> f64 {
        self.click_hold_min_ms
            + gauss(rng).abs().min(1.0) * 0.5 * (self.click_hold_max_ms - self.click_hold_min_ms)
    }

    /// A human per-key dwell (ms), right-skewed.
    fn key_dwell(&self, rng: &mut Rng) -> f64 {
        self.key_dwell_min_ms
            + gauss(rng).abs().min(1.0) * 0.45 * (self.key_dwell_max_ms - self.key_dwell_min_ms)
    }
}

/// One sample of a planned pointer path: absolute CSS coordinates and a post-step sleep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MoveStep {
    pub x: f64,
    pub y: f64,
    pub sleep_ms: f64,
}

/// One primitive of a planned typing sequence, dispatched in order.
#[derive(Clone, Debug, PartialEq)]
pub enum TypeOp {
    ShiftDown,
    ShiftUp,
    KeyDown {
        key: String,
        code: String,
        vk: i64,
        text: String,
        shift: bool,
    },
    KeyUp {
        key: String,
        code: String,
        vk: i64,
        shift: bool,
    },
    /// Fallback for a character no physical key produces (composed/zero-width/emoji): inserted
    /// atomically so the value is never corrupted.
    InsertText(String),
    Sleep(u64),
}

/// The tracked hand for one browser page: where the cursor is and the identity's cadence.
pub struct Humanizer {
    rng: Rng,
    persona: Persona,
    x: f64,
    y: f64,
    placed: bool,
    kb_warmed: bool,
}

impl Humanizer {
    /// Build a hand from an identity seed (`None` draws a fresh one per launch).
    pub fn new(seed: Option<&str>) -> Self {
        let seed_int = match seed {
            Some(seed) if !seed.trim().is_empty() => hash_seed(seed.trim()),
            _ => {
                let bytes = uuid::Uuid::new_v4().into_bytes();
                u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
        };
        let mut rng = Rng::new(seed_int);
        // A page starts with the pointer at the origin; that is itself a landmark, so seed the hand
        // somewhere plausible in the upper-left reading area.
        let x = rand_f64(&mut rng, 140.0, 380.0);
        let y = rand_f64(&mut rng, 90.0, 240.0);
        Self {
            rng,
            persona: Persona::from_seed(seed_int),
            x,
            y,
            placed: false,
            kb_warmed: false,
        }
    }

    /// Where the cursor currently is.
    pub fn cursor(&self) -> (f64, f64) {
        (self.x, self.y)
    }

    /// `true` once a real move has put the pointer somewhere.
    pub fn is_placed(&self) -> bool {
        self.placed
    }

    /// Plan a move to `to`, updating the tracked cursor to the endpoint.
    pub fn plan_move_to(&mut self, to: (f64, f64), target_w: f64) -> Vec<MoveStep> {
        let steps = plan_move((self.x, self.y), to, &self.persona, &mut self.rng, target_w, false);
        self.x = to.0;
        self.y = to.1;
        self.placed = true;
        steps
    }

    /// Plan a click at `to`: the approach path and the mousedown→mouseup hold, in ms.
    pub fn plan_click_to(&mut self, to: (f64, f64), target_w: f64) -> (Vec<MoveStep>, f64) {
        let steps = plan_move((self.x, self.y), to, &self.persona, &mut self.rng, target_w, false);
        self.x = to.0;
        self.y = to.1;
        self.placed = true;
        let hold = self.persona.click_hold(&mut self.rng);
        (steps, hold)
    }

    /// Plan a human click on `box = (x, y, width, height)`: pick a human point inside the box, glide
    /// there, and return the point, the approach path, and the mousedown→mouseup hold in ms.
    pub fn plan_click_box(&mut self, box_: (f64, f64, f64, f64)) -> ((f64, f64), Vec<MoveStep>, f64) {
        let from = (self.x, self.y);
        let target = click_point(box_, from, &mut self.rng);
        let steps = plan_move(from, target, &self.persona, &mut self.rng, 24.0, false);
        self.x = target.0;
        self.y = target.1;
        self.placed = true;
        let hold = self.persona.click_hold(&mut self.rng);
        (target, steps, hold)
    }

    /// Plan a non-goal ambient burst inside `viewport` lasting roughly `ms`.
    pub fn plan_ambient(&mut self, viewport: (f64, f64), ms: f64) -> Vec<MoveStep> {
        let steps = plan_ambient((self.x, self.y), viewport, &self.persona, &mut self.rng, ms);
        if let Some(last) = steps.last() {
            self.x = last.x;
            self.y = last.y;
        }
        self.placed = self.placed || !steps.is_empty();
        steps
    }

    /// Whether the keyboard is about to be used by a session whose mouse has never moved.
    pub fn needs_warmup(&self) -> bool {
        !self.kb_warmed && !self.placed
    }

    /// Consume the warmup: an ambient burst so a keydown stream is never the first input event.
    pub fn take_warmup(&mut self, viewport: (f64, f64)) -> Vec<MoveStep> {
        self.kb_warmed = true;
        if self.placed {
            return Vec::new();
        }
        let ms = rand_f64(&mut self.rng, 280.0, 620.0);
        self.plan_ambient(viewport, ms)
    }

    /// Plan a key-by-key typing sequence for `text`.
    ///
    /// Fat-finger typos are intentionally not simulated. The correction path has to read the field
    /// back to know what to delete, and a mail sender that deletes a real character from a recipient
    /// or a body is a correctness bug, not a realism win. Everything else a keystroke-dynamics
    /// collector measures — hold time, flight time, rollover, thinking pauses, Shift handling — is
    /// reproduced.
    pub fn plan_typing(&mut self, text: &str) -> Vec<TypeOp> {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let mut ops: Vec<TypeOp> = Vec::new();
        if n == 0 {
            return ops;
        }
        let roll = self.persona.rollover;
        let mut shift = false;
        let mut i = 0usize;
        while i < n {
            let ch = chars[i];
            let spec = char_key_spec(ch);
            let need_shift = spec.as_ref().map(|s| s.shift).unwrap_or(false);
            if need_shift && !shift {
                ops.push(TypeOp::ShiftDown);
                shift = true;
                ops.push(TypeOp::Sleep(rand_int(&mut self.rng, 35, 120) as u64));
            }

            let mut rolled = false;
            if !shift && i + 1 < n {
                if let (Some(cur), Some(next)) = (char_key_spec(ch), char_key_spec(chars[i + 1])) {
                    if !next.shift
                        && is_roll_key(ch)
                        && is_roll_key(chars[i + 1])
                        && ch != chars[i + 1]
                        && self.rng.next_f64() < roll
                    {
                        let d1 = self.persona.key_dwell(&mut self.rng);
                        let d2 = self.persona.key_dwell(&mut self.rng);
                        if d1 - 8.0 >= 12.0 {
                            let overlap = rand_f64(&mut self.rng, 12.0, (d1 - 8.0).min(55.0));
                            ops.push(TypeOp::KeyDown {
                                key: cur.key.clone(),
                                code: cur.code.clone(),
                                vk: cur.vk,
                                text: cur.text.clone(),
                                shift: false,
                            });
                            ops.push(TypeOp::Sleep((d1 - overlap).max(0.0) as u64));
                            ops.push(TypeOp::KeyDown {
                                key: next.key.clone(),
                                code: next.code.clone(),
                                vk: next.vk,
                                text: next.text.clone(),
                                shift: false,
                            });
                            ops.push(TypeOp::Sleep(overlap as u64));
                            ops.push(TypeOp::KeyUp {
                                key: cur.key.clone(),
                                code: cur.code.clone(),
                                vk: cur.vk,
                                shift: false,
                            });
                            ops.push(TypeOp::Sleep((d2 - overlap).max(5.0) as u64));
                            ops.push(TypeOp::KeyUp {
                                key: next.key.clone(),
                                code: next.code.clone(),
                                vk: next.vk,
                                shift: false,
                            });
                            rolled = true;
                            i += 1;
                        }
                    }
                }
            }

            if !rolled {
                match spec {
                    Some(s) => {
                        let dwell = self.persona.key_dwell(&mut self.rng);
                        ops.push(TypeOp::KeyDown {
                            key: s.key.clone(),
                            code: s.code.clone(),
                            vk: s.vk,
                            text: s.text.clone(),
                            shift,
                        });
                        ops.push(TypeOp::Sleep(dwell.max(1.0) as u64));
                        ops.push(TypeOp::KeyUp {
                            key: s.key,
                            code: s.code,
                            vk: s.vk,
                            shift,
                        });
                    }
                    None => {
                        // Batch a run of unmappable characters (zero-width marks, emoji, non-Latin
                        // scripts) into one atomic insert so the value is exact and fast.
                        let mut run = String::new();
                        run.push(ch);
                        let mut j = i + 1;
                        while j < n && char_key_spec(chars[j]).is_none() {
                            run.push(chars[j]);
                            j += 1;
                        }
                        ops.push(TypeOp::InsertText(run));
                        i = j - 1;
                    }
                }
            }

            let next_need_shift = if i + 1 < n {
                char_key_spec(chars[i + 1]).map(|s| s.shift).unwrap_or(false)
            } else {
                false
            };
            if shift && !next_need_shift {
                ops.push(TypeOp::Sleep(rand_int(&mut self.rng, 10, 70) as u64));
                ops.push(TypeOp::ShiftUp);
                shift = false;
            }

            if i + 1 < n {
                if self.rng.next_f64() < 0.06 {
                    ops.push(TypeOp::Sleep(rand_int(&mut self.rng, 180, 450) as u64));
                } else {
                    ops.push(TypeOp::Sleep(rand_int(&mut self.rng, 45, 150) as u64));
                }
                if chars[i].is_whitespace() {
                    ops.push(TypeOp::Sleep(rand_int(&mut self.rng, 20, 100) as u64));
                }
            }
            i += 1;
        }
        if shift {
            ops.push(TypeOp::ShiftUp);
        }
        ops
    }
}

/// Sample ONE minimum-jerk submovement `a→b` along a slightly bowed path, appending coloured noise.
/// `t0` is the absolute clock (ms) at the start; returns the clock at the end.
#[allow(clippy::too_many_arguments)]
fn sample_submove(
    out: &mut Vec<MoveStep>,
    a: (f64, f64),
    b: (f64, f64),
    dur_ms: f64,
    p: &Persona,
    rng: &mut Rng,
    t0: f64,
    drift: &mut (f64, f64),
    jit: &mut (f64, f64),
    tremor_phase: f64,
    land: bool,
) -> f64 {
    let dx = b.0 - a.0;
    let dy = b.1 - a.1;
    let dist = (dx * dx + dy * dy).sqrt().max(1e-6);
    let step_ms = 1000.0 / p.device_hz;
    let n = ((dur_ms / step_ms).round() as i64).clamp(4, 180);
    let nx = -dy / dist;
    let ny = dx / dist;
    let bow_sign = if p.approach_bias >= 0.0 { 1.0 } else { -1.0 };
    let bow = bow_sign
        * (0.04 + gauss(rng).abs() * 0.05)
        * dist.min(260.0)
        * (0.5 + p.approach_bias.abs());
    let c1x = a.0 + dx * 0.33 + nx * bow;
    let c1y = a.1 + dy * 0.33 + ny * bow;
    let c2x = a.0 + dx * 0.66 + nx * bow;
    let c2y = a.1 + dy * 0.66 + ny * bow;
    let mut t = t0;
    for i in 1..=n {
        let e = min_jerk(i as f64 / n as f64);
        let mut px = bez(a.0, c1x, c2x, b.0, e);
        let mut py = bez(a.1, c1y, c2y, b.1, e);
        let last = i == n;
        if !(last && land) {
            let k = step_ms / p.drift_tau;
            drift.0 += -k * drift.0 + (2.0 * k).sqrt() * p.drift_amp * gauss(rng);
            drift.1 += -k * drift.1 + (2.0 * k).sqrt() * p.drift_amp * gauss(rng);
            let t_amp = p.tremor_amp * (0.7 + 0.3 * ((2.0 * PI * t) / 850.0 + tremor_phase).sin());
            let tr_x = t_amp * (tremor_phase + (2.0 * PI * p.tremor_hz * t) / 1000.0).sin();
            let tr_y = t_amp
                * (tremor_phase + 1.0 + (2.0 * PI * (p.tremor_hz * 0.93) * t) / 1000.0).sin();
            jit.0 = 0.7 * jit.0 + gauss(rng) * p.jitter * 0.22;
            jit.1 = 0.7 * jit.1 + gauss(rng) * p.jitter * 0.22;
            px += drift.0 + tr_x + jit.0;
            py += drift.1 + tr_y + jit.1;
        } else {
            px = b.0;
            py = b.1;
        }
        // Device sampling is regular (a sharp cadence mode): keep per-sample jitter small.
        let sleep_ms = step_ms * lerp(0.88, 1.12, rng.next_f64());
        out.push(MoveStep { x: px, y: py, sleep_ms });
        t += sleep_ms;
    }
    t
}

/// Plan a free cursor move as a sum of minimum-jerk submovements (primary + corrections) with
/// Fitts-scaled duration and coloured noise.
fn plan_move(
    from: (f64, f64),
    to: (f64, f64),
    p: &Persona,
    rng: &mut Rng,
    target_w: f64,
    settle: bool,
) -> Vec<MoveStep> {
    let w = target_w.max(6.0);
    let dx = to.0 - from.0;
    let dy = to.1 - from.1;
    let d = (dx * dx + dy * dy).sqrt();
    let mut out: Vec<MoveStep> = Vec::new();
    if d < 1.5 {
        out.push(MoveStep {
            x: to.0,
            y: to.1,
            sleep_ms: 1000.0 / p.device_hz,
        });
        return out;
    }
    let id = (d / w + 1.0).log2();
    let mt_lo = 1700.0_f64.min(70.0_f64.max(40.0 + d * 0.6));
    let mt = ((p.fitts_a + p.fitts_b * id) * (0.85 + 0.3 * rng.next_f64())).clamp(mt_lo, 1700.0);

    let mut targets: Vec<(f64, f64)> = Vec::new();
    if d >= 40.0 {
        let f = p.primary_frac;
        let spread = 16.0_f64.min(d * p.overshoot);
        targets.push((
            from.0 + (to.0 - from.0) * f + gauss(rng) * spread,
            from.1 + (to.1 - from.1) * f + gauss(rng) * spread,
        ));
        let n_corr = if rng.next_f64() < 0.08 * p.max_corrections as f64 {
            2
        } else {
            1
        };
        let mut cur = targets[0];
        for _ in 0..n_corr {
            let close = lerp(0.55, 0.8, rng.next_f64());
            let mut nx = (
                cur.0 + (to.0 - cur.0) * close,
                cur.1 + (to.1 - cur.1) * close,
            );
            nx.0 += gauss(rng) * 1.2;
            nx.1 += gauss(rng) * 1.2;
            targets.push(nx);
            cur = nx;
        }
    }
    targets.push(to);

    let mut seg_dur: Vec<f64> = Vec::new();
    if targets.len() == 1 {
        seg_dur.push(mt);
    } else {
        seg_dur.push(mt * 0.78);
        let rest = mt * 0.22;
        let nc = targets.len() - 1;
        for _ in 0..nc {
            seg_dur.push((rest / nc as f64) * lerp(0.8, 1.2, rng.next_f64()));
        }
    }

    let mut drift = (0.0f64, 0.0f64);
    let mut jit = (0.0f64, 0.0f64);
    let tremor_phase = rng.next_f64() * 2.0 * PI;
    let mut a = from;
    let mut t = 0.0f64;
    for s in 0..targets.len() {
        let b = targets[s];
        let last = s == targets.len() - 1;
        t = sample_submove(
            &mut out,
            a,
            b,
            seg_dur[s].max(20.0),
            p,
            rng,
            t,
            &mut drift,
            &mut jit,
            tremor_phase,
            last && !settle,
        );
        if !last {
            if let Some(last_step) = out.last_mut() {
                last_step.sleep_ms += lerp(40.0, 120.0, rng.next_f64());
            }
        }
        a = b;
    }
    if settle {
        let jig = 1 + (rng.next_f64() * 2.0) as i64;
        for _ in 0..jig {
            out.push(MoveStep {
                x: to.0 + gauss(rng) * 1.4,
                y: to.1 + gauss(rng) * 1.4,
                sleep_ms: lerp(30.0, 90.0, rng.next_f64()),
            });
        }
        out.push(MoveStep {
            x: to.0,
            y: to.1,
            sleep_ms: 1000.0 / p.device_hz,
        });
    }
    out
}

/// Plan ambient / pre-challenge pointer activity: idle drift plus a few non-goal moves inside the
/// comfortable central band, so a collector sees non-zero pointer entropy before the first goal action.
fn plan_ambient(
    from: (f64, f64),
    viewport: (f64, f64),
    p: &Persona,
    rng: &mut Rng,
    ms: f64,
) -> Vec<MoveStep> {
    let mut out: Vec<MoveStep> = Vec::new();
    let mut cur = from;
    let mut elapsed = 0.0f64;
    let budget = ms.max(200.0);
    let bx0 = viewport.0 * 0.12;
    let bx1 = viewport.0 * 0.88;
    let by0 = viewport.1 * 0.14;
    let by1 = viewport.1 * 0.8;
    while elapsed < budget && out.len() < 4000 {
        let target = (rand_f64(rng, bx0, bx1), rand_f64(rng, by0, by1));
        let seg = plan_move(cur, target, p, rng, 80.0, false);
        for step in &seg {
            out.push(*step);
            elapsed += step.sleep_ms;
        }
        let rest_ms = lerp(120.0, 600.0, rng.next_f64());
        let mut drift = (0.0f64, 0.0f64);
        let step_ms = 1000.0 / p.device_hz;
        let mut held = 0.0f64;
        let tphase = rng.next_f64() * 2.0 * PI;
        while held < rest_ms {
            let k = step_ms / p.drift_tau;
            drift.0 += -k * drift.0 + (2.0 * k).sqrt() * p.drift_amp * gauss(rng);
            drift.1 += -k * drift.1 + (2.0 * k).sqrt() * p.drift_amp * gauss(rng);
            let ph = tphase + (2.0 * PI * p.tremor_hz * held) / 1000.0;
            out.push(MoveStep {
                x: target.0 + drift.0 + p.tremor_amp * ph.sin(),
                y: target.1 + drift.1 + p.tremor_amp * (ph + PI / 2.0).sin(),
                sleep_ms: step_ms * lerp(0.9, 1.4, rng.next_f64()),
            });
            held += step_ms;
            elapsed += step_ms;
        }
        cur = target;
    }
    out
}

/// A human click point inside `box = (x, y, width, height)`: a 2-D gaussian around the centre nudged
/// toward the approach side (people undershoot toward where the cursor came from), clamped a couple
/// of pixels inside the edges.
pub fn click_point(box_: (f64, f64, f64, f64), from: (f64, f64), rng: &mut Rng) -> (f64, f64) {
    let (bx, by, bw, bh) = box_;
    let cx = bx + bw / 2.0;
    let cy = by + bh / 2.0;
    let dx = cx - from.0;
    let dy = cy - from.1;
    let d = (dx * dx + dy * dy).sqrt().max(1.0);
    let ux = -dx / d;
    let uy = -dy / d;
    let x = cx + ux * bw * 0.12 + gauss(rng) * bw * 0.16;
    let y = cy + uy * bh * 0.12 + gauss(rng) * bh * 0.16;
    let x = if bw >= 4.0 { x.clamp(bx + 2.0, bx + bw - 2.0) } else { cx };
    let y = if bh >= 4.0 { y.clamp(by + 2.0, by + bh - 2.0) } else { cy };
    (x, y)
}

/// A resolved physical key for one character.
struct KeySpec {
    key: String,
    code: String,
    vk: i64,
    text: String,
    shift: bool,
}

/// The keys a person rolls over: lower-case, unshifted, one physical key each.
fn is_roll_key(ch: char) -> bool {
    matches!(ch,
        'a'..='z' | '0'..='9' | ' ' | ',' | '.' | ';' | '\'' | '/' | '-' | '=')
}

/// The US-layout key a character is typed with, or `None` when no physical key produces it.
fn char_key_spec(ch: char) -> Option<KeySpec> {
    let make = |key: &str, code: &str, vk: i64, text: char, shift: bool| KeySpec {
        key: key.to_string(),
        code: code.to_string(),
        vk,
        text: text.to_string(),
        shift,
    };
    let spec = match ch {
        'a'..='z' => make(
            &ch.to_string(),
            &format!("Key{}", ch.to_ascii_uppercase()),
            ch.to_ascii_uppercase() as i64,
            ch,
            false,
        ),
        'A'..='Z' => make(&ch.to_string(), &format!("Key{ch}"), ch as i64, ch, true),
        '0'..='9' => make(&ch.to_string(), &format!("Digit{ch}"), ch as i64, ch, false),
        ' ' => make(" ", "Space", 32, ch, false),
        '\n' => KeySpec {
            key: "Enter".to_string(),
            code: "Enter".to_string(),
            vk: 13,
            text: "\r".to_string(),
            shift: false,
        },
        '\t' => make("Tab", "Tab", 9, ch, false),
        '`' => make("`", "Backquote", 192, ch, false),
        '~' => make("~", "Backquote", 192, ch, true),
        '-' => make("-", "Minus", 189, ch, false),
        '_' => make("_", "Minus", 189, ch, true),
        '=' => make("=", "Equal", 187, ch, false),
        '+' => make("+", "Equal", 187, ch, true),
        '[' => make("[", "BracketLeft", 219, ch, false),
        '{' => make("{", "BracketLeft", 219, ch, true),
        ']' => make("]", "BracketRight", 221, ch, false),
        '}' => make("}", "BracketRight", 221, ch, true),
        '\\' => make("\\", "Backslash", 220, ch, false),
        '|' => make("|", "Backslash", 220, ch, true),
        ';' => make(";", "Semicolon", 186, ch, false),
        ':' => make(":", "Semicolon", 186, ch, true),
        '\'' => make("'", "Quote", 222, ch, false),
        '"' => make("\"", "Quote", 222, ch, true),
        ',' => make(",", "Comma", 188, ch, false),
        '<' => make("<", "Comma", 188, ch, true),
        '.' => make(".", "Period", 190, ch, false),
        '>' => make(">", "Period", 190, ch, true),
        '/' => make("/", "Slash", 191, ch, false),
        '?' => make("?", "Slash", 191, ch, true),
        '!' => make("!", "Digit1", 49, ch, true),
        '@' => make("@", "Digit2", 50, ch, true),
        '#' => make("#", "Digit3", 51, ch, true),
        '$' => make("$", "Digit4", 52, ch, true),
        '%' => make("%", "Digit5", 53, ch, true),
        '^' => make("^", "Digit6", 54, ch, true),
        '&' => make("&", "Digit7", 55, ch, true),
        '*' => make("*", "Digit8", 56, ch, true),
        '(' => make("(", "Digit9", 57, ch, true),
        ')' => make(")", "Digit0", 48, ch, true),
        _ => return None,
    };
    Some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic_and_in_range() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            let value = a.next_f64();
            assert!((0.0..1.0).contains(&value));
            assert_eq!(value, b.next_f64());
        }
    }

    #[test]
    fn persona_draw_is_stable_per_seed() {
        let a = Persona::from_seed(hash_seed("novamail-seed"));
        let b = Persona::from_seed(hash_seed("novamail-seed"));
        let c = Persona::from_seed(hash_seed("other-seed"));
        assert_eq!(a.device_hz, b.device_hz);
        assert_eq!(a.rollover, b.rollover);
        assert!(a.rollover >= 0.12 && a.rollover <= 0.38);
        assert!(c.rollover >= 0.12 && c.rollover <= 0.38);
    }

    #[test]
    fn move_path_lands_on_target_and_is_bounded() {
        let mut hand = Humanizer::new(Some("seed-1"));
        let steps = hand.plan_move_to((900.0, 600.0), 24.0);
        assert!(!steps.is_empty());
        assert!(steps.len() <= 180 * 4 + 8);
        let last = steps.last().unwrap();
        assert!((last.x - 900.0).abs() < 1e-6);
        assert!((last.y - 600.0).abs() < 1e-6);
        assert_eq!(hand.cursor(), (900.0, 600.0));
        assert!(hand.is_placed());
    }

    #[test]
    fn click_point_stays_inside_the_box() {
        let mut rng = Rng::new(7);
        for _ in 0..200 {
            let (x, y) = click_point((100.0, 50.0, 120.0, 40.0), (0.0, 0.0), &mut rng);
            assert!(x >= 102.0 && x <= 218.0);
            assert!(y >= 52.0 && y <= 88.0);
        }
    }

    #[test]
    fn typing_covers_every_printable_ascii_char() {
        let mut hand = Humanizer::new(Some("seed-a"));
        for code in 32u8..127 {
            let ch = code as char;
            let ops = hand.plan_typing(&ch.to_string());
            assert!(
                !ops.is_empty(),
                "no ops produced for printable char {code:?}"
            );
            assert!(
                char_key_spec(ch).is_some() || matches!(ops[0], TypeOp::InsertText(_)),
                "unexpected plan for {ch:?}"
            );
        }
    }

    #[test]
    fn typing_a_recipient_emits_key_events() {
        let mut hand = Humanizer::new(Some("seed-b"));
        let ops = hand.plan_typing("user.name+tag@example.co.uk");
        assert!(ops.iter().any(|op| matches!(op, TypeOp::KeyDown { .. })));
        assert!(ops.iter().any(|op| matches!(op, TypeOp::ShiftDown)));
        assert!(ops.iter().any(|op| matches!(op, TypeOp::ShiftUp)));
        assert!(!ops.iter().any(|op| matches!(op, TypeOp::InsertText(_))));
    }

    #[test]
    fn typing_batches_unmappable_text_atomically() {
        let mut hand = Humanizer::new(Some("seed-c"));
        let invisible = "\u{200b}\u{feff}\u{200c}";
        let ops = hand.plan_typing(invisible);
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0], TypeOp::InsertText(invisible.to_string()));
    }
}
