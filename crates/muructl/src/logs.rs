//! Lines the host and its workers wrote, kept for the dashboard.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// How many lines are kept.
const CAPACITY: usize = 2000;

/// A bounded ring of recent log lines, shared between the relay tasks that
/// fill it and the dashboard that reads it.
#[derive(Clone, Default)]
pub struct Logs {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl Logs {
    /// Keep a line, dropping the oldest when full.
    pub fn push(&self, line: String) {
        let mut lines = self.lines.lock().expect("logs poisoned");
        if lines.len() == CAPACITY {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// The most recent lines, oldest first.
    pub fn recent(&self, limit: usize) -> Vec<String> {
        let lines = self.lines.lock().expect("logs poisoned");
        lines.iter().rev().take(limit).rev().cloned().collect()
    }
}
