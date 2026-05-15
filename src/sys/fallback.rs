//! Portable backend built on `tokio::time::sleep_until`.
//!
//! Used on targets without a native backend, when the `os-native` feature
//! is disabled, and as the substrate of M0 before the platform backends
//! land.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::time::{sleep_until, Instant, Sleep};

pub(super) struct Timer {
    sleep: Option<Pin<Box<Sleep>>>,
}

impl Timer {
    pub(super) fn new() -> Self {
        Self { sleep: None }
    }

    pub(super) fn arm(&mut self, deadline: Instant) {
        match self.sleep.as_mut() {
            Some(s) => s.as_mut().reset(deadline),
            None => self.sleep = Some(Box::pin(sleep_until(deadline))),
        }
    }

    pub(super) fn disarm(&mut self) {
        self.sleep = None;
    }

    pub(super) fn poll_expired(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match self.sleep.as_mut() {
            Some(s) => s.as_mut().poll(cx),
            None => Poll::Pending,
        }
    }
}
