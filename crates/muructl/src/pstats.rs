//! pstats output for `muructl debug profile --format pstats` (design
//! specification section 11.7): the merged samples as the statistics
//! Python's pstats module, snakeviz and the like load, in Python's
//! marshal format.
//!
//! The figures come from samples, not from counting calls: a function's
//! calls are the samples it appeared in, its own time the samples it was
//! running in, and its cumulative time the samples it was on the stack
//! for, each sample worth one interval.

use std::collections::{BTreeMap, BTreeSet};

/// A function as pstats names it: file, first line and name.
type Function = (String, u32, String);

/// What pstats holds for a function or for one of its callers.
#[derive(Default, Clone, Copy)]
struct Figures {
    calls: u64,
    own: f64,
    cumulative: f64,
}

#[derive(Default)]
struct Entry {
    figures: Figures,
    callers: BTreeMap<Function, Figures>,
}

/// A frame's label, `qualname (file:line)`, as a pstats function.
fn function(label: &str) -> Function {
    label
        .strip_suffix(')')
        .and_then(|rest| rest.rsplit_once(" ("))
        .and_then(|(name, place)| {
            let (file, line) = place.rsplit_once(':')?;
            Some((file.to_owned(), line.parse().ok()?, name.to_owned()))
        })
        .unwrap_or_else(|| ("~".to_owned(), 0, label.to_owned()))
}

/// The statistics of collapsed stacks (`thread;root;...;leaf` and a
/// count), each sample worth `interval` seconds, as marshal bytes.
pub fn encode<'a>(stacks: impl IntoIterator<Item = (&'a str, u64)>, interval: f64) -> Vec<u8> {
    let mut entries: BTreeMap<Function, Entry> = BTreeMap::new();

    for (stack, count) in stacks {
        let frames: Vec<Function> = stack.split(';').skip(1).map(function).collect();
        let time = count as f64 * interval;

        let Some(leaf) = frames.last() else {
            continue;
        };

        entries.entry(leaf.clone()).or_default().figures.own += time;

        // A function or a call recursing within one stack counts once
        // for it.

        let mut counted = BTreeSet::new();

        for frame in &frames {
            if counted.insert(frame) {
                let figures = &mut entries.entry(frame.clone()).or_default().figures;
                figures.calls += count;
                figures.cumulative += time;
            }
        }

        let mut edges = BTreeSet::new();

        for (index, pair) in frames.windows(2).enumerate() {
            if !edges.insert((&pair[0], &pair[1])) {
                continue;
            }

            let caller = entries
                .entry(pair[1].clone())
                .or_default()
                .callers
                .entry(pair[0].clone())
                .or_default();
            caller.calls += count;
            caller.cumulative += time;

            if index + 2 == frames.len() {
                caller.own += time;
            }
        }
    }

    let mut out = vec![b'{'];

    for (function, entry) in &entries {
        write_function(&mut out, function);
        out.extend_from_slice(&[b')', 5]);
        write_int(&mut out, entry.figures.calls);
        write_int(&mut out, entry.figures.calls);
        write_float(&mut out, entry.figures.own);
        write_float(&mut out, entry.figures.cumulative);
        out.push(b'{');

        for (caller, figures) in &entry.callers {
            write_function(&mut out, caller);
            out.extend_from_slice(&[b')', 4]);
            write_int(&mut out, figures.calls);
            write_int(&mut out, figures.calls);
            write_float(&mut out, figures.own);
            write_float(&mut out, figures.cumulative);
        }

        out.push(b'0');
    }

    out.push(b'0');
    out
}

fn write_function(out: &mut Vec<u8>, (file, line, name): &Function) {
    out.extend_from_slice(&[b')', 3]);
    write_text(out, file);
    write_int(out, u64::from(*line));
    write_text(out, name);
}

fn write_int(out: &mut Vec<u8>, value: u64) {
    out.push(b'i');
    out.extend_from_slice(&i32::try_from(value).unwrap_or(i32::MAX).to_le_bytes());
}

fn write_float(out: &mut Vec<u8>, value: f64) {
    out.push(b'g');
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_text(out: &mut Vec<u8>, text: &str) {
    out.push(b'u');
    out.extend_from_slice(&u32::try_from(text.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_become_pstats_functions() {
        assert_eq!(
            function("Shop.view (shop.py:12)"),
            ("shop.py".to_owned(), 12, "Shop.view".to_owned())
        );
        assert_eq!(function("odd"), ("~".to_owned(), 0, "odd".to_owned()));
    }

    #[test]
    fn samples_are_counted_once_per_function_and_call() {
        let bytes = encode(
            [
                ("t;a (m.py:1);b (m.py:5)", 3),
                ("t;a (m.py:1);a (m.py:1)", 1),
            ],
            0.01,
        );
        assert_eq!(bytes[0], b'{');
        assert_eq!(*bytes.last().unwrap(), b'0');
    }
}
