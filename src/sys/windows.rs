//! Windows backend with two implementations selected at runtime:
//!
//! 1. **High-resolution waitable timer** (Windows 10 1803+ / Server 2019+).
//!    Uses [`CreateWaitableTimerExW`] with
//!    [`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`] and integrates with the
//!    Win32 threadpool wait subsystem via [`CreateThreadpoolWait`]. This
//!    gives ~0.5 ms resolution comparable to a Linux `timerfd`, without
//!    raising the system-wide tick rate.
//!
//! 2. **Threadpool timer** (universal fallback). Uses
//!    [`CreateThreadpoolTimer`] directly. Resolution is bounded by the
//!    system tick (~15.6 ms by default).
//!
//! Selection is performed once per process via a probe stored in a
//! `OnceLock<bool>`. If the high-resolution flag is rejected (older
//! Windows), every subsequent [`Timer::new`] uses the threadpool-timer
//! path.
//!
//! Each `OsInterval` owns its own kernel object, armed as a one-shot;
//! the userspace `MissedTickBehavior` policy in [`crate::interval`] is
//! responsible for re-arming each tick.

use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tokio::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, TRUE};
use windows_sys::Win32::System::Threading::{
    CancelWaitableTimer, CloseThreadpoolTimer, CloseThreadpoolWait, CreateThreadpoolTimer,
    CreateThreadpoolWait, CreateWaitableTimerExW, SetThreadpoolTimer, SetThreadpoolWait,
    SetWaitableTimer, WaitForThreadpoolTimerCallbacks, WaitForThreadpoolWaitCallbacks,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, PTP_CALLBACK_INSTANCE, PTP_TIMER, PTP_WAIT,
    TIMER_ALL_ACCESS,
};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Shared state between a kernel-side callback and the awaiting task.
struct State {
    /// Set by the callback when the timer expires; cleared on observation
    /// (in `poll`) and on `arm`/`disarm`.
    fired: AtomicBool,
    /// Waker registered by the most recent `poll` returning Pending.
    waker: Mutex<Option<Waker>>,
}

impl State {
    fn new() -> Self {
        Self {
            fired: AtomicBool::new(false),
            waker: Mutex::new(None),
        }
    }

    /// Called from a threadpool callback. Marks the timer as fired and
    /// wakes any pending task.
    fn fire(&self) {
        self.fired.store(true, Ordering::Release);
        let waker = self.waker.lock().expect("waker mutex poisoned").take();
        if let Some(w) = waker {
            w.wake();
        }
    }

    fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        if self.fired.swap(false, Ordering::Acquire) {
            return Poll::Ready(());
        }
        let mut slot = self.waker.lock().expect("waker mutex poisoned");
        // Re-check under the lock to close the race against a callback
        // that fires between the swap above and acquiring the lock here.
        if self.fired.swap(false, Ordering::Acquire) {
            return Poll::Ready(());
        }
        match slot.as_ref() {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => *slot = Some(cx.waker().clone()),
        }
        Poll::Pending
    }

    /// Clear any prior fire/waker so a fresh `arm` starts from a clean state.
    fn reset(&self) {
        self.fired.store(false, Ordering::Release);
        *self.waker.lock().expect("waker mutex poisoned") = None;
    }
}

// ---------------------------------------------------------------------------
// Runtime probe for high-res waitable-timer support
// ---------------------------------------------------------------------------

/// Returns `true` if `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` is supported
/// on this system (Windows 10 1803+ / Server 2019+). Probed once per
/// process and cached.
fn high_res_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| unsafe {
        let h = CreateWaitableTimerExW(
            ptr::null(),
            ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        );
        if h.is_null() {
            false
        } else {
            CloseHandle(h);
            true
        }
    })
}

// ---------------------------------------------------------------------------
// High-resolution backend (CreateWaitableTimerExW + CreateThreadpoolWait)
// ---------------------------------------------------------------------------

struct HighRes {
    htimer: HANDLE,
    pwait: PTP_WAIT,
    state: Arc<State>,
    ctx: *const State,
}

// SAFETY: Both `HANDLE` and `PTP_WAIT` are opaque kernel handles whose
// associated APIs are documented thread-safe. `ctx` is a raw pointer into
// an `Arc<State>` we own a strong count for; all shared mutation goes
// through `Arc<State>`, which is `Send + Sync`.
unsafe impl Send for HighRes {}

impl HighRes {
    fn new(state: Arc<State>) -> Option<Self> {
        if !high_res_supported() {
            return None;
        }
        unsafe {
            let htimer = CreateWaitableTimerExW(
                ptr::null(),
                ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            );
            if htimer.is_null() {
                return None;
            }
            let ctx: *const State = Arc::into_raw(Arc::clone(&state));
            let pwait = CreateThreadpoolWait(Some(wait_callback), ctx as *mut c_void, ptr::null());
            if pwait == 0 {
                drop(Arc::from_raw(ctx));
                CloseHandle(htimer);
                return None;
            }
            Some(Self {
                htimer,
                pwait,
                state,
                ctx,
            })
        }
    }

    fn arm(&mut self, deadline: Instant) {
        let delta = deadline.saturating_duration_since(Instant::now());
        let due = relative_due_time(delta);
        // Clear any prior fire/waker before associating the new wait.
        self.state.reset();
        unsafe {
            // Auto-reset (synchronization) timer: the wait subsystem
            // resets the signal when it observes it. lPeriod = 0 means
            // one-shot; pfnCompletionRoutine = None (we use a wait, not
            // an APC); fResume = FALSE.
            SetWaitableTimer(self.htimer, &due, 0, None, ptr::null(), 0);
            // Associate (or re-associate) the wait with the timer. NULL
            // pftTimeout means wait indefinitely.
            SetThreadpoolWait(self.pwait, self.htimer, ptr::null());
        }
    }

    /// Cancel any pending wakeup. See [`Timer::disarm`] for the
    /// "synchronous wait inside async" caveat.
    fn disarm(&mut self) {
        unsafe {
            // Detach the wait first so no new callback can be queued from
            // a subsequent timer signal.
            SetThreadpoolWait(self.pwait, ptr::null_mut(), ptr::null());
            CancelWaitableTimer(self.htimer);
            // Wait for any in-flight callback to finish so no stale fire
            // lands after we return.
            WaitForThreadpoolWaitCallbacks(self.pwait, TRUE);
        }
        self.state.reset();
    }

    fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.state.poll(cx)
    }
}

impl Drop for HighRes {
    fn drop(&mut self) {
        unsafe {
            SetThreadpoolWait(self.pwait, ptr::null_mut(), ptr::null());
            CancelWaitableTimer(self.htimer);
            WaitForThreadpoolWaitCallbacks(self.pwait, TRUE);
            CloseThreadpoolWait(self.pwait);
            CloseHandle(self.htimer);
            // No more callbacks can fire — safe to reclaim the strong
            // count we handed out in `new`.
            drop(Arc::from_raw(self.ctx));
        }
    }
}

unsafe extern "system" fn wait_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _wait: PTP_WAIT,
    _wait_result: u32,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: `context` is the pointer produced by `Arc::into_raw` in
    // `HighRes::new`. We only borrow the underlying `State`; the strong
    // count is reclaimed by `Drop for HighRes` after
    // `WaitForThreadpoolWaitCallbacks` ensures no callback is running.
    let state: &State = unsafe { &*context.cast::<State>() };
    state.fire();
}

// ---------------------------------------------------------------------------
// Fallback backend (CreateThreadpoolTimer)
// ---------------------------------------------------------------------------

struct Pool {
    handle: PTP_TIMER,
    state: Arc<State>,
    ctx: *const State,
}

// SAFETY: see `unsafe impl Send for HighRes`; identical reasoning applies
// to `PTP_TIMER`. Not `Sync`: `poll_expired` takes `&mut self`.
unsafe impl Send for Pool {}

impl Pool {
    fn new(state: Arc<State>) -> Self {
        let ctx: *const State = Arc::into_raw(Arc::clone(&state));
        let handle =
            unsafe { CreateThreadpoolTimer(Some(timer_callback), ctx as *mut c_void, ptr::null()) };
        if handle == 0 {
            // Reclaim the strong count we handed to the (failed) callback ctx.
            let err = std::io::Error::last_os_error();
            unsafe {
                drop(Arc::from_raw(ctx));
            }
            panic!("CreateThreadpoolTimer failed: {err}");
        }
        Self { handle, state, ctx }
    }

    fn arm(&mut self, deadline: Instant) {
        let delta = deadline.saturating_duration_since(Instant::now());
        let ft = relative_filetime(delta);
        // Clear any prior fire/waker before arming the new one-shot.
        self.state.reset();
        unsafe {
            SetThreadpoolTimer(self.handle, &ft, 0, 0);
        }
    }

    fn disarm(&mut self) {
        unsafe {
            // NULL pftDueTime cancels any pending one-shot.
            SetThreadpoolTimer(self.handle, ptr::null(), 0, 0);
            // Cancel queued callbacks and wait for any in-flight to finish
            // so no stale fire can land after we return.
            WaitForThreadpoolTimerCallbacks(self.handle, TRUE);
        }
        self.state.reset();
    }

    fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.state.poll(cx)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            SetThreadpoolTimer(self.handle, ptr::null(), 0, 0);
            WaitForThreadpoolTimerCallbacks(self.handle, TRUE);
            CloseThreadpoolTimer(self.handle);
            drop(Arc::from_raw(self.ctx));
        }
    }
}

unsafe extern "system" fn timer_callback(
    _instance: PTP_CALLBACK_INSTANCE,
    context: *mut c_void,
    _timer: PTP_TIMER,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: see `wait_callback`; identical reasoning applies.
    let state: &State = unsafe { &*context.cast::<State>() };
    state.fire();
}

// ---------------------------------------------------------------------------
// Public Timer dispatch
// ---------------------------------------------------------------------------

enum Inner {
    HighRes(HighRes),
    Pool(Pool),
}

pub(super) struct Timer(Inner);

impl Timer {
    pub(super) fn new() -> Self {
        let state = Arc::new(State::new());
        if let Some(hr) = HighRes::new(Arc::clone(&state)) {
            return Self(Inner::HighRes(hr));
        }
        Self(Inner::Pool(Pool::new(state)))
    }

    /// Arm the one-shot timer for `deadline`.
    ///
    /// Invariant: callers must only invoke `arm` after either (a) the
    /// previous arm has been observed via `poll_expired` returning
    /// `Ready`, or (b) `disarm` has been called. The steady-state caller
    /// in `OsInterval::poll_tick` satisfies (a); reset paths satisfy (b).
    pub(super) fn arm(&mut self, deadline: Instant) {
        match &mut self.0 {
            Inner::HighRes(t) => t.arm(deadline),
            Inner::Pool(t) => t.arm(deadline),
        }
    }

    /// Cancel any pending wakeup.
    ///
    /// Synchronously waits for any in-flight threadpool callback to
    /// complete. The callback body is trivially short (a couple of
    /// atomics + a waker wake), so this is effectively non-blocking, but
    /// it is a synchronous wait inside an async context — only call from
    /// cold paths (`reset_*`, `Drop`).
    pub(super) fn disarm(&mut self) {
        match &mut self.0 {
            Inner::HighRes(t) => t.disarm(),
            Inner::Pool(t) => t.disarm(),
        }
    }

    pub(super) fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match &mut self.0 {
            Inner::HighRes(t) => t.poll_expired(cx),
            Inner::Pool(t) => t.poll_expired(cx),
        }
    }
}

// ---------------------------------------------------------------------------
// Time conversion helpers
// ---------------------------------------------------------------------------

/// Compute a *relative* `LARGE_INTEGER` due time (suitable for
/// `SetWaitableTimer`'s `lpDueTime`). Per MSDN, a negative value means
/// "this many 100-ns intervals from now"; a non-negative value would be
/// interpreted as an absolute UTC FILETIME (origin: Jan 1, 1601), which
/// we don't want.
fn relative_due_time(d: Duration) -> i64 {
    let hundred_ns = d.as_nanos() / 100;
    // Floor at 1 so the value remains strictly negative after negation
    // and is unambiguously a relative time.
    let hundred_ns = if hundred_ns == 0 { 1 } else { hundred_ns };
    clamp_to_i64(hundred_ns).wrapping_neg()
}

/// Build a `FILETIME` representing the same relative due time, for the
/// `SetThreadpoolTimer` API which takes a `*const FILETIME`.
fn relative_filetime(d: Duration) -> FILETIME {
    // Bit-cast i64 → u64 so the split into two u32s is unambiguous and
    // doesn't depend on `as` truncation of a sign-extended shift.
    #[allow(clippy::cast_sign_loss)]
    let bits = relative_due_time(d) as u64;
    #[allow(clippy::cast_possible_truncation)]
    FILETIME {
        dwLowDateTime: bits as u32,
        dwHighDateTime: (bits >> 32) as u32,
    }
}

fn clamp_to_i64(value: u128) -> i64 {
    if value > i64::MAX as u128 {
        i64::MAX
    } else {
        #[allow(clippy::cast_possible_truncation)]
        {
            value as i64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reassemble a `FILETIME` into the same i64 that `SetWaitableTimer`
    /// would receive via `lpDueTime`.
    fn filetime_to_i64(ft: FILETIME) -> i64 {
        ((u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)) as i64
    }

    #[test]
    fn relative_due_time_zero_is_minus_one() {
        // Zero must floor to 1 hundred-ns tick, then negate, so that the
        // value is strictly negative (relative) and never zero/positive
        // (which Windows would interpret as an absolute FILETIME).
        assert_eq!(relative_due_time(Duration::ZERO), -1);
    }

    #[test]
    fn relative_due_time_sub_hundred_ns_floors_to_minus_one() {
        // 50 ns is < 100 ns, so hundred_ns == 0 → floored to 1 → -1.
        assert_eq!(relative_due_time(Duration::from_nanos(50)), -1);
    }

    #[test]
    fn relative_due_time_one_hundred_ns_unit() {
        assert_eq!(relative_due_time(Duration::from_nanos(100)), -1);
        assert_eq!(relative_due_time(Duration::from_nanos(200)), -2);
    }

    #[test]
    fn relative_due_time_one_millisecond() {
        // 1 ms = 1_000_000 ns = 10_000 hundred-ns units.
        assert_eq!(relative_due_time(Duration::from_millis(1)), -10_000);
    }

    #[test]
    fn relative_due_time_one_second() {
        // 1 s = 10_000_000 hundred-ns units.
        assert_eq!(relative_due_time(Duration::from_secs(1)), -10_000_000);
    }

    #[test]
    fn relative_due_time_is_always_negative() {
        for d in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_nanos(99),
            Duration::from_nanos(100),
            Duration::from_micros(1),
            Duration::from_millis(1),
            Duration::from_secs(1),
            Duration::from_secs(60 * 60 * 24),
        ] {
            assert!(
                relative_due_time(d) < 0,
                "relative_due_time({d:?}) was not negative",
            );
        }
    }

    #[test]
    fn relative_due_time_clamps_huge_durations() {
        // Far beyond i64::MAX hundred-ns units: must clamp to
        // -i64::MAX (wrapping_neg of i64::MAX), never overflow or
        // become non-negative.
        let huge = Duration::new(u64::MAX, 999_999_999);
        let v = relative_due_time(huge);
        assert_eq!(v, (i64::MAX).wrapping_neg());
        assert!(v < 0);
    }

    #[test]
    fn relative_filetime_matches_relative_due_time() {
        for d in [
            Duration::ZERO,
            Duration::from_nanos(50),
            Duration::from_nanos(100),
            Duration::from_micros(1),
            Duration::from_millis(1),
            Duration::from_millis(250),
            Duration::from_secs(1),
            Duration::from_secs(3600),
        ] {
            let ft = relative_filetime(d);
            assert_eq!(
                filetime_to_i64(ft),
                relative_due_time(d),
                "FILETIME for {d:?} did not round-trip to the same i64",
            );
        }
    }

    #[test]
    fn relative_filetime_one_millisecond_split() {
        // -10_000 as u64 = 0xFFFF_FFFF_FFFF_D8F0
        let ft = relative_filetime(Duration::from_millis(1));
        assert_eq!(ft.dwHighDateTime, 0xFFFF_FFFF);
        assert_eq!(ft.dwLowDateTime, 0xFFFF_D8F0);
    }

    #[test]
    fn relative_filetime_zero_is_minus_one_split() {
        // -1 as u64 = 0xFFFF_FFFF_FFFF_FFFF
        let ft = relative_filetime(Duration::ZERO);
        assert_eq!(ft.dwHighDateTime, 0xFFFF_FFFF);
        assert_eq!(ft.dwLowDateTime, 0xFFFF_FFFF);
    }
}
