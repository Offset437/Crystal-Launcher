//! The Viewer's request driver (app 1003e9b4), shared by every request Crystal
//! starts on the native core. Timeouts, retries and cancels follow the stock
//! rules exactly; see pol-spec/04-social.md §A.2 and 03-lifecycle.md §0.
//!
//! A cancel (337, or its wrappers 199/328) stamps a core-wide time that makes
//! every request poll report "pending" for 10 s, including FFXI's. Only the
//! Viewer's own cancel points cancel here: a request pending past its policy's
//! limit, or an owner that is shutting down (launcher gone, or the core's
//! latched pump error). A failed request is never cancelled: the core has
//! already released its ticket.

use crate::native_auth::Api;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Verified against polcore 73b1864b (preferred base 10000000).
pub const ENTRY_RVAS: &[(usize, usize)] = &[(1107, 0x19520), (337, 0x1fc30)];

/// One Crystal owner frame (07b decision 2: keep 20 ms). Not the stock period:
/// app.dll's ticker sleeps SleepEx(max(.., 20)) on the default 15.625 ms timer,
/// so a cold stock Viewer frame is 31.25 ms, and ≈21.4 ms only after FFXI has
/// run in the same pol.exe (FFXiMain leaks timeBeginPeriod(1)); pol-spec 08 §2.
pub(crate) const FRAME: Duration = Duration::from_millis(20);
/// The stock Viewer's cold frame period, in µs (08 §2, measured 2026-10-12).
const STOCK_FRAME_US: u64 = 31_250;
/// Crystal frames covering the wall time of `stock` cold stock frames, so a
/// frame-counted Viewer timer keeps its stock period at FRAME (07d §6 S1 D):
/// 3600 stock frames = 112.5 s = 5625 frames of 20 ms.
pub(crate) const fn stock_frames(stock: u32) -> u32 {
    (STOCK_FRAME_US * stock as u64 / FRAME.as_micros() as u64) as u32
}
/// The Viewer's mail timers (pending limit, retry wait): 3600 stock frames.
pub(crate) const MAIL_FRAMES: u32 = stock_frames(3600);
const MAIL_PERIOD: Duration = Duration::from_micros(STOCK_FRAME_US * 3600);
/// Slot 1107: nonzero when a native error is worth retrying.
const RETRYABLE: usize = 1107;
/// Slot 337: the core's request cancel (199 and 328 call the same code).
pub(crate) const CANCEL: usize = 337;
/// The Viewer's timeout error after cancelling a request pending too long.
pub(crate) const TIMED_OUT: i32 = -2046;
/// Not issued or abandoned: no signed-in member, or the owner is stopping.
pub(crate) const ABORTED: i32 = -1025;
const SLOTS_FULL: i32 = -256;
const MAX_RETRIES: u32 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Sign-in loads (flag 1): no timeout and no retry.
    Login,
    /// Put lists, status, group and comment requests (flag 0): cancel after
    /// 60 s pending, then up to 3 retries 30 s apart for retryable errors.
    Normal,
    /// Mail list/download/ack/send: cancel after 3600 stock frames pending
    /// (112.5 s cold), then up to 3 retries that far apart for retryable errors.
    Mail,
}

impl Policy {
    fn pending_limit(self) -> Option<Duration> {
        match self {
            Policy::Login => None,
            Policy::Normal => Some(Duration::from_secs(60)),
            Policy::Mail => Some(MAIL_PERIOD),
        }
    }
    fn backoff(self) -> Duration {
        match self {
            Policy::Login | Policy::Normal => Duration::from_secs(30),
            Policy::Mail => MAIL_PERIOD,
        }
    }
}

// --- request lines for measurement (pol-spec 07d §4.7) ----------------------

/// The `+S.SSs` origin: the sign-in, or the latest return from a game.
static REQUEST_CLOCK: Mutex<Option<Instant>> = Mutex::new(None);
pub(crate) fn restart_request_clock() {
    *REQUEST_CLOCK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
}
fn request_clock() -> Duration {
    REQUEST_CLOCK.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Instant::now).elapsed()
}

/// Clean (loss-free) durations from the 2026-10-12 captures (07d §4.5).
const CLEAN_REQUEST: Duration = Duration::from_millis(1150);
pub(crate) const CLEAN_LISTS: Duration = Duration::from_millis(1100);
fn clean(name: &str) -> Option<Duration> {
    matches!(name, "status" | "mail-list" | "presence").then_some(CLEAN_REQUEST)
}

/// One request's start line now and its end line on drop: `result`, the poll
/// count and the start attempts (full-table reissues and retries included),
/// then a possible-loss note where the step has a clean value. Owner thread
/// counters only; nothing here touches the core.
pub(crate) struct RequestLog {
    name: &'static str,
    started: Instant,
    pub(crate) polls: u32,
    pub(crate) attempts: u32,
    pub(crate) result: i32,
}
impl RequestLog {
    pub(crate) fn start(name: &'static str) -> Self {
        crate::log!("native POL request {name} start at +{:.2}s", request_clock().as_secs_f64());
        Self { name, started: Instant::now(), polls: 0, attempts: 0, result: ABORTED }
    }
}
impl Drop for RequestLog {
    fn drop(&mut self) {
        let took = self.started.elapsed();
        crate::log!("native POL request {} end {} after {:.2}s: {} polls, {} attempt(s)",
            self.name, self.result, took.as_secs_f64(), self.polls, self.attempts);
        if let Some(note) = clean(self.name).and_then(|clean| possible_loss(self.name, took, clean)) {
            crate::log!("{note}");
        }
    }
}

/// TCP timer quanta one lost packet adds to a step (08 §1 row 1), (low, high) s:
/// the SYN/DNS retry, Square Enix's SYN-ACK and data retransmissions, and the
/// latter doubled. Same table and format as the launcher's launch_timing.
const LOSS_QUANTA: [(f64, f64, &str); 4] = [
    (1.0, 1.0, "1.0 s SYN/DNS retry"),
    (3.36, 3.36, "3.36 s SYN-ACK retransmission"),
    (4.1, 4.55, "4.1-4.55 s server retransmission"),
    (8.2, 9.1, "8.2-9.1 s doubled server retransmission"),
];
const LOSS_SLACK: f64 = 0.15;

/// `possible packet loss: …` when `actual` overshoots `clean` by one quantum
/// or the sum of two, ±0.15 s (07d §4.5).
pub(crate) fn possible_loss(step: &str, actual: Duration, clean: Duration) -> Option<String> {
    let excess = actual.as_secs_f64() - clean.as_secs_f64();
    let fits = |low: f64, high: f64| excess >= low - LOSS_SLACK && excess <= high + LOSS_SLACK;
    let label = LOSS_QUANTA.iter().find(|(low, high, _)| fits(*low, *high)).map(|(.., name)| (*name).to_owned())
        .or_else(|| LOSS_QUANTA.iter().flat_map(|a| LOSS_QUANTA.iter().map(move |b| (a, b)))
            .find(|(a, b)| fits(a.0 + b.0, a.1 + b.1)).map(|(a, b)| format!("{} + {}", a.2, b.2)))?;
    Some(format!("possible packet loss: {step} took {:.2} s, {excess:+.2} s over its clean {:.2} s ({label})",
        actual.as_secs_f64(), clean.as_secs_f64()))
}

pub(crate) struct Request<'a> {
    /// The request-line name (07d §4.7).
    pub name: &'static str,
    /// Starts the request: a ticket (>= 0) or a native error.
    pub start: &'a mut dyn FnMut() -> i32,
    /// Poll slot taking the ticket: 0 pending, > 0 done, < 0 native error.
    pub poll: usize,
    pub policy: Policy,
}

/// Runs one request to completion on the owner thread. `frame` is called once
/// between polls (pump 818, dispatch window messages, sleep `FRAME`); it
/// returns false when the owner is stopping, which cancels only this request's
/// own unfinished ticket.
///
/// Returns the native completion (> 0), -261 (completed without a result, as
/// the Viewer treats it), or the final native error / `TIMED_OUT` / `ABORTED`.
pub(crate) fn run(api: &Api, request: Request<'_>, frame: &mut dyn FnMut() -> bool) -> i32 {
    run_all(api, &mut [request], frame, &mut |_, _| {})[0]
}

/// Queue related Viewer jobs together. Every job gets one state step per
/// frame, its own timeout and retries, and its own completion notification.
/// A slow or failed job cannot delay starting or completing another one.
pub(crate) fn run_all(
    api: &Api,
    requests: &mut [Request<'_>],
    frame: &mut dyn FnMut() -> bool,
    completed: &mut dyn FnMut(usize, i32),
) -> Vec<i32> {
    run_all_with(requests, frame, &mut |slot, argument| api.one(slot, argument),
        &mut || api.zero(838) == 3, Instant::now, completed)
}

#[derive(Clone, Copy)]
enum State {
    Start,
    Poll { ticket: i32, since: Instant },
    Retry { error: i32, since: Instant },
    Done(i32),
}

pub(crate) struct Job {
    state: State,
    retries: u32,
    /// From the first start call to Done (07d §4.7).
    log: Option<RequestLog>,
}

impl Job {
    pub(crate) fn new() -> Self { Self { state: State::Start, retries: 0, log: None } }

    /// Advance an owned job once without running a nested owner loop.
    pub(crate) fn advance(&mut self, api: &Api, request: &mut Request<'_>) -> Option<i32> {
        if matches!(self.state, State::Done(_)) { return None; }
        self.step(request, &mut |slot, argument| api.one(slot, argument),
            &mut || api.zero(838) == 3, Instant::now());
        self.done()
    }

    /// A ticket is out: the core has the request and only a poll (or the
    /// policy limit's cancel) finishes it. Play waits only on these.
    pub(crate) fn in_flight(&self) -> bool { matches!(self.state, State::Poll { .. }) }

    /// The Viewer job destructor/replacement cancels only its pending ticket.
    pub(crate) fn cancel(&mut self, api: &Api) {
        if let State::Poll { ticket, .. } = self.state { api.one(CANCEL, ticket as usize); }
        self.state = State::Done(ABORTED);
        self.done();
    }

    /// The final code once Done, writing the request's end line once.
    fn done(&mut self) -> Option<i32> {
        let State::Done(code) = self.state else { return None };
        if let Some(mut log) = self.log.take() { log.result = code; }
        Some(code)
    }

    fn retry(&mut self, policy: Policy, error: i32, now: Instant) {
        if policy == Policy::Login {
            self.state = State::Done(error);
            return;
        }
        self.retries += 1;
        self.state = if self.retries > MAX_RETRIES {
            State::Done(error)
        } else {
            State::Retry { error, since: now }
        };
    }

    fn step(
        &mut self,
        request: &mut Request<'_>,
        call: &mut dyn FnMut(usize, usize) -> i32,
        signed_in: &mut dyn FnMut() -> bool,
        now: Instant,
    ) {
        match self.state {
            State::Start if !signed_in() => self.state = State::Done(ABORTED),
            State::Start => match {
                self.log.get_or_insert_with(|| RequestLog::start(request.name)).attempts += 1;
                (request.start)()
            } {
                ticket if ticket >= 0 => self.state = State::Poll { ticket, since: now },
                SLOTS_FULL => {}, // allocation failed: try next frame without a retry
                code @ (-261 | -263) => self.state = State::Done(code),
                error => self.retry(request.policy, error, now),
            },
            State::Poll { ticket, since } => {
                if let Some(log) = self.log.as_mut() { log.polls += 1; }
                let result = call(request.poll, ticket as usize);
                if result > 0 || result == -261 {
                    self.state = State::Done(result);
                } else if result < 0 {
                    // A failed native poll has already released its ticket.
                    if call(RETRYABLE, result as usize) == 0 {
                        self.state = State::Done(result);
                    } else {
                        self.retry(request.policy, result, now);
                    }
                } else if request.policy.pending_limit().is_some_and(|limit| now.duration_since(since) > limit) {
                    call(CANCEL, ticket as usize);
                    if request.policy == Policy::Mail && call(RETRYABLE, TIMED_OUT as usize) == 0 {
                        self.state = State::Done(TIMED_OUT);
                    } else {
                        self.retry(request.policy, TIMED_OUT, now);
                    }
                }
            }
            // Once Play is requested no new attempt starts (07a
            // presign-no-cancel-freeze 1): the failure stands, nothing is sent.
            State::Retry { error, .. } if crate::native_auth::play_requested() => self.state = State::Done(error),
            State::Retry { error, since } => {
                if error == SLOTS_FULL || now.duration_since(since) > request.policy.backoff() {
                    self.state = State::Start;
                }
            }
            State::Done(_) => {},
        }
    }
}

fn run_all_with(
    requests: &mut [Request<'_>],
    frame: &mut dyn FnMut() -> bool,
    call: &mut dyn FnMut(usize, usize) -> i32,
    signed_in: &mut dyn FnMut() -> bool,
    mut now: impl FnMut() -> Instant,
    completed: &mut dyn FnMut(usize, i32),
) -> Vec<i32> {
    let mut jobs: Vec<_> = requests.iter().map(|_| Job::new()).collect();
    loop {
        for (index, (job, request)) in jobs.iter_mut().zip(requests.iter_mut()).enumerate() {
            if matches!(job.state, State::Done(_)) { continue; }
            job.step(request, call, signed_in, now());
            if let Some(code) = job.done() { completed(index, code); }
        }
        if jobs.iter().all(|job| matches!(job.state, State::Done(_))) { break; }
        if !frame() {
            for (index, job) in jobs.iter_mut().enumerate() {
                if matches!(job.state, State::Done(_)) { continue; }
                if let State::Poll { ticket, .. } = job.state { call(CANCEL, ticket as usize); }
                job.state = State::Done(ABORTED);
                job.done();
                completed(index, ABORTED);
            }
            break;
        }
    }
    jobs.into_iter().map(|job| match job.state { State::Done(code) => code, _ => unreachable!() }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    struct Fake {
        starts: Vec<i32>,
        polls: Vec<i32>,
        retryable: bool,
        cancels: Vec<usize>,
        started: u32,
    }

    fn drive(fake: &RefCell<Fake>, policy: Policy, frames: Option<u32>) -> i32 {
        let mut remaining = frames;
        let mut start = || { let mut f = fake.borrow_mut(); f.started += 1; f.starts.remove(0) };
        let mut frame = || match remaining.as_mut() {
            Some(0) => false,
            Some(n) => { *n -= 1; true }
            None => true,
        };
        let mut call = |slot: usize, argument: usize| -> i32 {
            let mut f = fake.borrow_mut();
            match slot {
                RETRYABLE => i32::from(f.retryable),
                CANCEL => { f.cancels.push(argument); 0 }
                _ => if f.polls.is_empty() { 0 } else { f.polls.remove(0) },
            }
        };
        run_all_with(&mut [Request { name: "test", start: &mut start, poll: 164, policy }], &mut frame,
            &mut call, &mut || true, Instant::now, &mut |_, _| {})[0]
    }

    fn fake(starts: &[i32], polls: &[i32], retryable: bool) -> RefCell<Fake> {
        RefCell::new(Fake { starts: starts.to_vec(), polls: polls.to_vec(), retryable, cancels: Vec::new(), started: 0 })
    }

    #[test]
    fn completion_and_native_errors_never_cancel() {
        let f = fake(&[3], &[0, 0, 1], false);
        assert_eq!(drive(&f, Policy::Normal, None), 1);
        assert!(f.borrow().cancels.is_empty());
        let f = fake(&[3], &[-5326], false);
        assert_eq!(drive(&f, Policy::Normal, None), -5326);
        assert!(f.borrow().cancels.is_empty());
    }

    #[test]
    fn full_request_table_reissues_next_frame_without_counting_a_retry() {
        let f = fake(&[-256, -256, 2], &[1], false);
        assert_eq!(drive(&f, Policy::Login, None), 1);
        assert_eq!(f.borrow().started, 3);
    }

    #[test]
    fn login_loads_return_errors_without_retrying() {
        let f = fake(&[-515], &[], true);
        assert_eq!(drive(&f, Policy::Login, None), -515);
        let f = fake(&[1], &[-515], true);
        assert_eq!(drive(&f, Policy::Login, None), -515);
        assert_eq!(f.borrow().started, 1);
    }

    #[test]
    fn stopping_owner_cancels_only_its_own_pending_ticket() {
        let f = fake(&[7], &[], false);
        assert_eq!(drive(&f, Policy::Normal, Some(2)), ABORTED);
        assert_eq!(f.borrow().cancels, [7]);
        // A request that already failed has no ticket left to cancel.
        let f = fake(&[-515], &[], true);
        assert_eq!(drive(&f, Policy::Normal, Some(0)), ABORTED);
        assert!(f.borrow().cancels.is_empty());
    }

    #[test]
    fn concurrent_jobs_start_together_and_report_completion_independently() {
        let calls = RefCell::new(Vec::new());
        let mut first = || { calls.borrow_mut().push((376, 10)); 10 };
        let mut second = || { calls.borrow_mut().push((404, 20)); 20 };
        let mut jobs = [
            Request { name: "test", start: &mut first, poll: 377, policy: Policy::Normal },
            Request { name: "test", start: &mut second, poll: 405, policy: Policy::Normal },
        ];
        let frames = Cell::new(0);
        let mut frame = || { frames.set(frames.get() + 1); frames.get() < 2 };
        let mut completed = Vec::new();
        let results = run_all_with(&mut jobs, &mut frame, &mut |slot, ticket| {
            calls.borrow_mut().push((slot, ticket));
            if slot == 405 { 1 } else { 0 }
        }, &mut || true, Instant::now, &mut |index, code| completed.push((index, code)));
        assert_eq!(results, [ABORTED, 1]);
        assert_eq!(completed, [(1, 1), (0, ABORTED)]);
        assert_eq!(*calls.borrow(), [(376, 10), (404, 20), (377, 10), (405, 20), (CANCEL, 10)]);
    }

    #[test]
    fn retry_backoff_in_one_job_does_not_delay_a_successful_sibling() {
        let start = Instant::now();
        let clock = Cell::new(start);
        let attempts = RefCell::new(Vec::new());
        let mut first = || { attempts.borrow_mut().push(clock.get().duration_since(start)); 10 };
        let mut second = || 20;
        let mut jobs = [
            Request { name: "test", start: &mut first, poll: 377, policy: Policy::Normal },
            Request { name: "test", start: &mut second, poll: 405, policy: Policy::Normal },
        ];
        let mut completed = Vec::new();
        let results = run_all_with(&mut jobs,
            &mut || { clock.set(clock.get() + Duration::from_secs(1)); true },
            &mut |slot, _| if slot == 377 { -515 } else { 1 },
            &mut || true, || clock.get(), &mut |index, code| completed.push((index, code, clock.get())));
        assert_eq!(results, [-515, 1]);
        assert_eq!((completed[0].0, completed[0].1), (1, 1));
        assert_eq!(completed[0].2.duration_since(start), Duration::from_secs(1));
        let attempts = attempts.borrow();
        assert_eq!(attempts.len(), 4, "initial attempt and at most three retries");
        assert!(attempts.windows(2).all(|pair| pair[1] - pair[0] > Duration::from_secs(30)));
    }

    #[test]
    fn after_play_an_in_flight_ticket_finishes_or_times_out_but_nothing_is_retried() {
        let _play = crate::native_auth::TestPlay::requested();
        // In flight: polled to completion, never cancelled.
        let f = fake(&[3], &[0, 0, 1], true);
        assert_eq!(drive(&f, Policy::Normal, None), 1);
        assert!(f.borrow().cancels.is_empty());
        // A retryable failure is final: no second start.
        let f = fake(&[3, 4], &[-515], true);
        assert_eq!(drive(&f, Policy::Normal, None), -515);
        assert_eq!(f.borrow().started, 1);
        // The stock 60 s policy cancel still fires; its retry does not start.
        let start = Instant::now();
        let clock = Cell::new(start);
        let attempts = Cell::new(0);
        let mut first = || { attempts.set(attempts.get() + 1); 10 };
        let mut canceled = Vec::new();
        let results = run_all_with(&mut [Request { name: "test", start: &mut first, poll: 377, policy: Policy::Normal }],
            &mut || { clock.set(clock.get() + Duration::from_secs(61)); true },
            &mut |slot, ticket| { if slot == CANCEL { canceled.push(ticket); } 0 },
            &mut || true, || clock.get(), &mut |_, _| {});
        assert_eq!(results, [TIMED_OUT]);
        assert_eq!(canceled, [10]);
        assert_eq!(attempts.get(), 1);
    }

    /// 07d §6 S1 D: the Viewer's 3600-frame mail timers keep their cold stock
    /// wall time (3600 × 31.25 ms = 112.5 s), not 3600 × 20 ms = 72 s.
    #[test]
    fn mail_policy_cancels_and_retries_at_the_stock_cold_period() {
        assert_eq!((MAIL_PERIOD, MAIL_FRAMES, stock_frames(3600)), (Duration::from_millis(112_500), 5625, 5625));
        assert_eq!(FRAME * MAIL_FRAMES, MAIL_PERIOD);
        let start = Instant::now();
        let clock = Cell::new(start);
        let starts = RefCell::new(Vec::new());
        let cancels = RefCell::new(Vec::new());
        let mut first = || { starts.borrow_mut().push(clock.get().duration_since(start)); 10 };
        let results = run_all_with(&mut [Request { name: "test", start: &mut first, poll: 327, policy: Policy::Mail }],
            &mut || { clock.set(clock.get() + Duration::from_millis(500)); true },
            &mut |slot, ticket| match slot {
                CANCEL => { cancels.borrow_mut().push(clock.get().duration_since(start)); let _ = ticket; 0 }
                RETRYABLE => 1,
                _ => 0,
            }, &mut || true, || clock.get(), &mut |_, _| {});
        assert_eq!(results, [TIMED_OUT]);
        let cancels = cancels.borrow();
        // Pending past 112.5 s (not at 72 s): the first cancel is at 113 s.
        assert_eq!(cancels[0], Duration::from_secs(113));
        let starts = starts.borrow();
        assert_eq!(starts.len(), 4, "the first attempt and three retries");
        // Each retry waits more than 112.5 s after its timeout.
        for (cancel, restart) in cancels.iter().zip(&starts[1..]) {
            assert!(*restart - *cancel > MAIL_PERIOD && *restart - *cancel <= MAIL_PERIOD + Duration::from_secs(1), "{cancel:?} → {restart:?}");
        }
    }

    #[test]
    fn possible_loss_matches_the_launcher_format_and_quanta() {
        let s = Duration::from_secs_f64;
        assert_eq!(possible_loss("status", s(5.25), s(1.15)).as_deref(),
            Some("possible packet loss: status took 5.25 s, +4.10 s over its clean 1.15 s (4.1-4.55 s server retransmission)"));
        assert!(possible_loss("presence", s(4.50), s(1.15)).unwrap().ends_with("(3.36 s SYN-ACK retransmission)"));
        assert!(possible_loss("lists", s(13.25), s(1.1)).unwrap().contains(" + 8.2-9.1 s doubled server retransmission)"));
        for actual in [1.30, 2.6, 2.9, 7.0] { assert_eq!(possible_loss("mail-list", s(actual), s(1.15)), None, "{actual}"); }
        assert_eq!(clean("status"), Some(s(1.15)));
        assert_eq!(clean("open-member"), None, "member login has no clean value");
    }

    #[test]
    fn timeout_cancels_only_the_pending_job_and_uses_its_own_retry_clock() {
        let start = Instant::now();
        let clock = Cell::new(start);
        let attempts = Cell::new(0);
        let mut first = || { attempts.set(attempts.get() + 1); 10 };
        let mut second = || 20;
        let mut jobs = [
            Request { name: "test", start: &mut first, poll: 377, policy: Policy::Normal },
            Request { name: "test", start: &mut second, poll: 405, policy: Policy::Normal },
        ];
        let mut canceled = Vec::new();
        let results = run_all_with(&mut jobs,
            &mut || { clock.set(clock.get() + Duration::from_secs(61)); true },
            &mut |slot, ticket| match slot {
                CANCEL => { canceled.push(ticket); 0 },
                377 if attempts.get() == 1 => 0,
                _ => 1,
            }, &mut || true, || clock.get(), &mut |_, _| {});
        assert_eq!(results, [1, 1]);
        assert_eq!(canceled, [10]);
        assert_eq!(attempts.get(), 2);
    }
}
