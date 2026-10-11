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
use std::time::{Duration, Instant};

// Verified against polcore 73b1864b (preferred base 10000000).
pub const ENTRY_RVAS: &[(usize, usize)] = &[(1107, 0x19520), (337, 0x1fc30)];

/// One Viewer UI frame. The Viewer's timer runs frames at least 20 ms apart.
pub(crate) const FRAME: Duration = Duration::from_millis(20);
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
    /// Mail list/download/ack/send: cancel after 3600 frames pending, then up
    /// to 3 retries 3600 frames apart for retryable errors.
    Mail,
}

impl Policy {
    fn pending_limit(self) -> Option<Duration> {
        match self {
            Policy::Login => None,
            Policy::Normal => Some(Duration::from_secs(60)),
            Policy::Mail => Some(FRAME * 3600),
        }
    }
    fn backoff(self) -> Duration {
        match self {
            Policy::Login | Policy::Normal => Duration::from_secs(30),
            Policy::Mail => FRAME * 3600,
        }
    }
}

pub(crate) struct Request<'a> {
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
    run_with(request, frame, &mut |slot, argument| api.one(slot, argument), &mut || api.zero(838) == 3, Instant::now)
}

fn run_with(
    request: Request<'_>,
    frame: &mut dyn FnMut() -> bool,
    call: &mut dyn FnMut(usize, usize) -> i32,
    signed_in: &mut dyn FnMut() -> bool,
    now: fn() -> Instant,
) -> i32 {
    let Request { start, poll, policy } = request;
    let mut retries = 0;
    let mut last;
    'issue: loop {
        // The Viewer does not issue a request without a connected member.
        if !signed_in() {
            return ABORTED;
        }
        let ticket = start();
        match ticket {
            t if t >= 0 => {}
            SLOTS_FULL => {
                if !frame() { return ABORTED; }
                continue 'issue;
            }
            -261 | -263 => return ticket,
            error => {
                last = error;
                if policy == Policy::Login { return error; }
                match wait_to_retry(policy, &mut retries, last, frame, now) {
                    Some(code) => return code,
                    None => continue 'issue,
                }
            }
        }
        let started = now();
        loop {
            let result = call(poll, ticket as usize);
            if result > 0 || result == -261 {
                return result;
            }
            if result < 0 {
                // The native poll has already released the ticket.
                if call(RETRYABLE, result as usize) == 0 || policy == Policy::Login && result != SLOTS_FULL {
                    return result;
                }
                if result == SLOTS_FULL {
                    if !frame() { return ABORTED; }
                    continue 'issue;
                }
                last = result;
                break;
            }
            if policy.pending_limit().is_some_and(|limit| now().duration_since(started) > limit) {
                call(CANCEL, ticket as usize);
                last = TIMED_OUT;
                // The Viewer's mail jobs retry a timeout only when 1107 allows it.
                if policy == Policy::Mail && call(RETRYABLE, TIMED_OUT as usize) == 0 {
                    return TIMED_OUT;
                }
                break;
            }
            if !frame() {
                call(CANCEL, ticket as usize);
                return ABORTED;
            }
        }
        if let Some(code) = wait_to_retry(policy, &mut retries, last, frame, now) {
            return code;
        }
    }
}

/// None means "issue again"; Some is the final result.
fn wait_to_retry(policy: Policy, retries: &mut u32, last: i32, frame: &mut dyn FnMut() -> bool, now: fn() -> Instant) -> Option<i32> {
    *retries += 1;
    if *retries > MAX_RETRIES {
        return Some(last);
    }
    let since = now();
    while last != SLOTS_FULL && now().duration_since(since) <= policy.backoff() {
        if !frame() { return Some(ABORTED); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

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
        run_with(Request { start: &mut start, poll: 164, policy }, &mut frame, &mut call, &mut || true, Instant::now)
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
}
