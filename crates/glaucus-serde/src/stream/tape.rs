// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A replayable parser-event tape, for resolving aliases without a node tree.
//!
//! The tree path resolves an alias by cloning an already-built `Node`. A
//! streaming path has no nodes, so it must replay the **events** that built the
//! anchored value instead. That difference is the whole reason this type exists.

// Nothing consumes the tape yet: #52 through #56 are its callers, and each lands
// with its own tests. Scoped to this module and tied to those issues rather than
// left open-ended -- delete it when #52 lands and the compiler will confirm it is
// no longer needed. (#42 carried the same allow and #43 removed it on schedule.)
#![allow(dead_code)]

use std::collections::HashMap;
use std::ops::Range;

use glaucus_core::error::{ParserConfig, Result};
use glaucus_core::parser::Parser;
use glaucus_core::parser::event::{Event, EventKind};

/// Returns the anchor an event declares, if any.
///
/// Only three event kinds can carry one. Reading it here — rather than making the
/// caller hand it back — is what lets [`Tape::next`] decide whether an event needs
/// recording *before* it is returned, which is what keeps the anchor-free path
/// allocation-free.
fn declared_anchor<'e>(event: &'e Event<'_>) -> Option<&'e str> {
    match &event.kind {
        EventKind::Scalar { anchor, .. }
        | EventKind::SequenceStart { anchor, .. }
        | EventKind::MappingStart { anchor, .. } => anchor.as_deref(),
        _ => None,
    }
}

/// Parser events, with the ability to replay an anchored span.
pub(crate) struct Tape<'a> {
    parser: Parser<'a>,
    /// Every recorded event, in one flat buffer.
    ///
    /// **Not a `Vec` per anchor.** Nested anchors — `&x {p: &y 1}` — then cost
    /// nothing extra, because `y`'s range sits *inside* `x`'s and the inner
    /// events are stored once. Per-anchor vectors would store them twice, and the
    /// duplication compounds with nesting depth.
    buffer: Vec<Event<'a>>,
    /// Anchor name to its half-open range in `buffer`.
    anchors: HashMap<String, Range<usize>>,
    /// Anchors whose value is still being read, innermost last.
    ///
    /// Recording continues while this is non-empty, so an outer anchor still open
    /// keeps collecting after an inner one closes.
    open: Vec<(String, usize)>,
}

impl<'a> Tape<'a> {
    /// Creates a tape over `input` with default parser configuration.
    pub(crate) fn new(input: &'a str) -> Self {
        Self::with_config(input, ParserConfig::default())
    }

    /// Creates a tape over `input` with caller-supplied parser configuration.
    pub(crate) fn with_config(input: &'a str, config: ParserConfig) -> Self {
        Self {
            parser: Parser::with_config(input, config),
            buffer: Vec::new(),
            anchors: HashMap::new(),
            open: Vec::new(),
        }
    }

    /// Yields the next event, recording it when it may later need replaying.
    ///
    /// Recording is **lazy**: the buffer stays empty until an anchor is actually
    /// seen. Most documents contain none, and allocating a tape for them would
    /// make every ordinary parse pay for a feature it does not use.
    ///
    /// An event is recorded when either an anchor scope is open — it is part of
    /// some anchored value — or the event itself declares an anchor, since a
    /// caller that then calls [`begin_anchor`](Self::begin_anchor) needs this
    /// event to be the first of the recorded span. Reading the anchor here is
    /// what makes that possible without buffering speculatively.
    pub(crate) fn next(&mut self) -> Option<Result<Event<'a>>> {
        let event = match self.parser.next_event()? {
            Ok(e) => e,
            Err(e) => return Some(Err(e)),
        };

        if !self.open.is_empty() || declared_anchor(&event).is_some() {
            self.buffer.push(event.clone());
        }

        Some(Ok(event))
    }

    /// Opens an anchor scope starting at the most recently yielded event.
    ///
    /// Call this immediately after [`next`](Self::next) returns an event whose
    /// anchor you intend to record. The event is already in the buffer — that is
    /// what the anchor check in `next` is for — so the span starts at it rather
    /// than after it, and a replay reproduces the whole value including the
    /// `SequenceStart` or `MappingStart` that gives it its shape.
    pub(crate) fn begin_anchor(&mut self, name: &str) {
        // `saturating_sub` rather than `- 1`: a caller that opens a scope without
        // a preceding event gets an empty span, not a panic on an attacker-shaped
        // document.
        let start = self.buffer.len().saturating_sub(1);
        self.open.push((name.to_owned(), start));
    }

    /// Closes the innermost open anchor scope and records its span.
    ///
    /// A repeated anchor name overwrites the earlier span, matching YAML: an
    /// anchor may be redefined, and a later alias refers to the most recent
    /// definition.
    pub(crate) fn end_anchor(&mut self) {
        if let Some((name, start)) = self.open.pop() {
            let end = self.buffer.len();
            self.anchors.insert(name, start..end);
        }
    }

    /// The events that built `name`, or `None` if it was never anchored.
    ///
    /// `None` rather than a panic: an undefined alias is a defect in the
    /// *document*, which arrives from outside, so it must be an error the caller
    /// can report rather than a crash.
    pub(crate) fn replay(&self, name: &str) -> Option<&[Event<'a>]> {
        let range = self.anchors.get(name)?;
        self.buffer.get(range.clone())
    }

    /// How many events have been recorded.
    ///
    /// Exists for the test that pins the anchor-free path at zero. That is a
    /// performance requirement of #49 — "no allocation on the anchor-free path" —
    /// and an untested performance requirement is an aspiration.
    pub(crate) const fn buffered_events(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::Tape;

    /// Drives the tape to exhaustion, opening and closing anchor scopes the way a
    /// consumer would: `begin_anchor` on an anchored event, `end_anchor` when that
    /// value's scope closes.
    fn drive(input: &str) -> Tape<'_> {
        use glaucus_core::parser::event::EventKind;

        let mut tape = Tape::new(input);
        // Depth at which each open anchor was declared, so the matching close is
        // recognised. A scalar anchor closes immediately; a collection anchor
        // closes at its own End event.
        let mut pending: Vec<usize> = Vec::new();
        let mut depth = 0usize;

        while let Some(Ok(event)) = tape.next() {
            let anchor = super::declared_anchor(&event).map(str::to_owned);

            match event.kind {
                EventKind::SequenceStart { .. } | EventKind::MappingStart { .. } => {
                    if let Some(name) = &anchor {
                        tape.begin_anchor(name);
                        pending.push(depth);
                    }
                    depth += 1;
                }
                EventKind::SequenceEnd | EventKind::MappingEnd => {
                    depth -= 1;
                    if pending.last() == Some(&depth) {
                        pending.pop();
                        tape.end_anchor();
                    }
                }
                EventKind::Scalar { .. } => {
                    if let Some(name) = &anchor {
                        tape.begin_anchor(name);
                        tape.end_anchor();
                    }
                }
                _ => {}
            }
        }
        tape
    }

    /// The performance requirement of #49, as a test rather than an aspiration:
    /// a document with no anchors must never allocate the buffer.
    #[test]
    fn an_anchor_free_document_records_nothing() {
        for input in [
            "a: 1\n",
            "a:\n  b:\n    - 1\n    - 2\n",
            "[1, 2, {a: b}]\n",
            "",
            "# comment only\n",
            "a: !!str 123\n",
        ] {
            let tape = drive(input);
            assert_eq!(
                tape.buffered_events(),
                0,
                "{input:?} has no anchors and must not be recorded"
            );
        }
    }

    #[test]
    fn a_scalar_anchor_records_and_replays() {
        let tape = drive("a: &x 1\nb: *x\n");
        let events = tape.replay("x").expect("x was anchored");
        assert_eq!(events.len(), 1, "a scalar anchor is one event");
        assert!(tape.buffered_events() > 0);
    }

    #[test]
    fn a_collection_anchor_replays_its_whole_span() {
        let tape = drive("a: &x [1, 2, 3]\nb: *x\n");
        let events = tape.replay("x").expect("x was anchored");
        // SequenceStart, three scalars, SequenceEnd.
        assert_eq!(events.len(), 5, "got {events:?}");
    }

    /// The reason the buffer is flat: `y` sits inside `x`, stored once.
    #[test]
    fn nested_anchors_share_one_buffer() {
        let tape = drive("a: &x {p: &y 1}\n");
        let x = tape.replay("x").expect("x").len();
        let y = tape.replay("y").expect("y").len();

        assert_eq!(y, 1, "y is the inner scalar");
        assert!(x > y, "x must contain y, got x={x} y={y}");
        assert_eq!(
            tape.buffered_events(),
            x,
            "the inner events must be stored once, not duplicated per anchor"
        );
    }

    /// An outer anchor still open must keep collecting after an inner one closes.
    #[test]
    fn recording_continues_until_the_last_scope_closes() {
        let tape = drive("a: &outer\n  inner: &in [1, 2]\n  after: 3\n");
        let outer = tape.replay("outer").expect("outer");
        let inner = tape.replay("in").expect("in");

        assert!(
            outer.len() > inner.len(),
            "outer must span past the inner anchor: outer={} inner={}",
            outer.len(),
            inner.len()
        );
    }

    #[test]
    fn an_undefined_alias_yields_none_not_a_panic() {
        let tape = drive("a: 1\n");
        assert!(tape.replay("nope").is_none());

        let tape = drive("a: &x 1\n");
        assert!(tape.replay("x").is_some());
        assert!(tape.replay("y").is_none());
    }

    /// YAML permits an anchor name to be redefined; a later alias refers to the
    /// most recent definition.
    #[test]
    fn a_redefined_anchor_replaces_the_earlier_span() {
        let tape = drive("a: &x 1\nb: &x [1, 2]\nc: *x\n");
        let events = tape.replay("x").expect("x");
        assert_eq!(events.len(), 4, "the sequence, not the earlier scalar");
    }

    #[test]
    fn end_anchor_without_a_matching_begin_is_a_no_op() {
        let mut tape = Tape::new("a: 1\n");
        tape.end_anchor();
        tape.end_anchor();
        assert_eq!(tape.buffered_events(), 0);
        assert!(tape.replay("anything").is_none());
    }
}
