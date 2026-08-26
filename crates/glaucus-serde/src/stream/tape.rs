// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A replayable parser-event tape, for resolving aliases without a node tree.
//!
//! The tree path resolves an alias by cloning an already-built `Node`. A
//! streaming path has no nodes, so it must replay the **events** that built the
//! anchored value instead. That difference is the whole reason this type exists.

use std::collections::HashMap;
use std::ops::Range;

use glaucus_core::error::{Error, ErrorKind, ParserConfig, Result};
use glaucus_core::limits::ResourceLimits;
use glaucus_core::parser::Parser;
use glaucus_core::parser::event::{Event, EventKind};
use glaucus_core::types::Span;

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
#[allow(dead_code)]
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
    /// Anchors whose value is still being read, innermost last: name, where the
    /// span starts, and the nesting depth the anchor was declared at.
    ///
    /// Recording continues while this is non-empty, so an outer anchor still open
    /// keeps collecting after an inner one closes. The depth is what says when a
    /// scope closes: a scalar anchor closes at once, a collection anchor at its
    /// matching end event.
    open: Vec<(String, usize, usize)>,
    /// Nesting depth of the events read from the parser.
    depth: usize,
    /// Spans still being replayed, innermost last.
    ///
    /// A stack rather than one cursor: an alias inside an anchored value expands
    /// while that value is itself being replayed.
    replaying: Vec<Range<usize>>,
    /// How many aliases have been expanded, and how many events that produced.
    ///
    /// Expansion is where a small document becomes a large one -- `&a`, then `*a`
    /// twice under `&b`, then `*b` twice under `&c` doubles at every level. The
    /// streaming path never materialises the result, so memory stays flat, but
    /// the time still grows exponentially. Both counters are charged before any
    /// replay begins.
    expansions: usize,
    expanded_events: usize,
    /// Limits governing the two counters above.
    limits: ResourceLimits,
}

#[allow(dead_code)]
impl<'a> Tape<'a> {
    /// Creates a tape over `input` with default parser configuration.
    pub(crate) fn new(input: &'a str) -> Self {
        Self::with_config(input, ParserConfig::default())
    }

    /// Creates a tape over `input` with caller-supplied parser configuration.
    pub(crate) fn with_config(input: &'a str, config: ParserConfig) -> Self {
        let limits = config.limits.clone();
        Self {
            limits,
            parser: Parser::with_config(input, config),
            buffer: Vec::new(),
            anchors: HashMap::new(),
            open: Vec::new(),
            depth: 0,
            replaying: Vec::new(),
            expansions: 0,
            expanded_events: 0,
        }
    }

    /// Yields the next event, resolving aliases and recording anchored spans.
    ///
    /// Recording is **lazy**: the buffer stays empty until an anchor is actually
    /// seen. Most documents contain none, and allocating a tape for them would
    /// make every ordinary parse pay for a feature it does not use.
    ///
    /// An alias never reaches the caller. It is replaced by the events of the
    /// value it names, so a consumer deserialises an aliased value with no
    /// knowledge that aliases exist. Doing this here rather than in the
    /// deserialiser is what keeps anchor bookkeeping in one place: the tape knows
    /// where every span starts and ends, and nothing else has to agree with it.
    pub(crate) fn next(&mut self) -> Option<Result<Event<'a>>> {
        loop {
            // `pull` reports where the event came from. Deciding that here
            // instead -- from `replaying.is_empty()` before the call -- reads an
            // exhausted frame that `pull` is about to discard, so the next PARSER
            // event is mistaken for a replayed one: it goes unrecorded and its
            // enclosing anchor scope never closes, surfacing later as a bogus
            // undefined alias. One source of truth avoids the ordering entirely.
            let (live, result) = self.pull()?;
            let event = match result {
                Ok(event) => event,
                Err(e) => return Some(Err(e)),
            };

            if live {
                self.record(&event);
            }

            // The ALIAS event is what gets recorded above, not its expansion.
            // Replaying a span that contains one therefore expands it again --
            // which is what makes `&z [*x]` reproduce `x` when `*z` is resolved.
            if let EventKind::Alias { name } = &event.kind {
                if let Err(e) = self.expand(name, event.span) {
                    return Some(Err(e));
                }
                continue;
            }

            if live {
                self.advance_scopes(&event);
            }
            return Some(Ok(event));
        }
    }

    /// Takes the next event, and reports whether it came from the parser.
    ///
    /// Retiring finished replays and answering "is this live?" belong together:
    /// the answer is only correct once the exhausted frames are gone, and any
    /// caller that asked separately would have to get the order right.
    fn pull(&mut self) -> Option<(bool, Result<Event<'a>>)> {
        while let Some(range) = self.replaying.last_mut() {
            if range.start < range.end {
                let index = range.start;
                range.start += 1;
                // In bounds by construction, so indexing cannot panic on a
                // hostile document: `buffer` is append-only and every range is
                // built as `start..buffer.len()`, so `end` can never outrun it.
                return Some((false, Ok(self.buffer[index].clone())));
            }
            self.replaying.pop();
        }
        Some((true, self.parser.next_event()?))
    }

    /// Buffers an event when it may later need replaying, and opens any scope it
    /// declares.
    fn record(&mut self, event: &Event<'a>) {
        let anchor = declared_anchor(event).map(str::to_owned);

        // Recorded when either a scope is open -- the event is part of some
        // anchored value -- or the event declares an anchor, since the span has
        // to start AT that event rather than after it.
        if !self.open.is_empty() || anchor.is_some() {
            self.buffer.push(event.clone());
        }

        if let Some(name) = anchor {
            let start = self.buffer.len().saturating_sub(1);
            self.open.push((name, start, self.depth));
        }
    }

    /// Tracks nesting depth and closes every scope the event completes.
    fn advance_scopes(&mut self, event: &Event<'a>) {
        match event.kind {
            EventKind::SequenceStart { .. } | EventKind::MappingStart { .. } => {
                self.depth += 1;
            }
            EventKind::SequenceEnd | EventKind::MappingEnd => {
                // `saturating_sub`: an unbalanced end event is a malformed
                // document, which arrives from outside. It must not underflow.
                self.depth = self.depth.saturating_sub(1);
            }
            _ => {}
        }

        // A scope closes once the stream is back at the depth it opened at: a
        // scalar anchor never changed the depth, so it closes immediately, while
        // a collection anchor waits for its matching end event.
        while self.open.last().is_some_and(|&(_, _, d)| d >= self.depth) {
            if let Some((name, start, _)) = self.open.pop() {
                // A repeated anchor name overwrites the earlier span, matching
                // YAML: an anchor may be redefined and a later alias refers to
                // the most recent definition.
                self.anchors.insert(name, start..self.buffer.len());
            }
        }
    }

    /// Queues the events of `name` for replay.
    ///
    /// An anchor still being read is deliberately absent from `anchors`, so
    /// `&x [*x]` reports an undefined alias instead of looping forever.
    fn expand(&mut self, name: &str, span: Span) -> Result<()> {
        let Some(range) = self.anchors.get(name).cloned() else {
            return Err(Error::new(
                ErrorKind::UndefinedAlias(name.to_string()),
                span,
            ));
        };

        // Charged BEFORE the replay is queued, so a document cannot spend the
        // budget and then be told it was over it.
        self.expansions += 1;
        if self.expansions > self.limits.max_alias_expansions {
            return Err(Error::new(
                ErrorKind::AliasExpansionLimitExceeded {
                    limit: self.limits.max_alias_expansions,
                },
                span,
            ));
        }

        self.expanded_events += range.len();
        if self.expanded_events > self.limits.max_total_alias_nodes {
            return Err(Error::new(
                ErrorKind::AliasMaterializationLimitExceeded {
                    limit: self.limits.max_total_alias_nodes,
                },
                span,
            ));
        }

        self.replaying.push(range);
        Ok(())
    }

    /// The events that built `name`, or `None` if it was never anchored.
    ///
    /// `None` rather than a panic: an undefined alias is a defect in the
    /// *document*, which arrives from outside, so it must be an error the caller
    /// can report rather than a crash.
    #[allow(dead_code)]
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
    use glaucus_core::error::ParserConfig;

    use super::Tape;

    /// Drives the tape to exhaustion.
    ///
    /// Nothing to co-operate with: the tape opens and closes anchor scopes
    /// itself, so a consumer only has to keep calling `next`.
    fn drive(input: &str) -> Tape<'_> {
        let mut tape = Tape::new(input);
        while let Some(Ok(_)) = tape.next() {}
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
    fn an_unbalanced_end_event_does_not_underflow_the_depth() {
        // A stray `]` is a malformed document, which arrives from outside. The
        // depth counter must saturate rather than wrap to usize::MAX and leave
        // every later anchor scope permanently open.
        for input in ["a: 1\n]\n", "]\n", "a: &x [1]\n]\n"] {
            let tape = drive(input);
            let _ = tape.buffered_events();
        }
    }

    /// Drives the tape to exhaustion, returning the first error it reports.
    fn drive_err(input: &str, config: ParserConfig) -> Option<String> {
        let mut tape = Tape::with_config(input, config);
        while let Some(result) = tape.next() {
            if let Err(e) = result {
                return Some(e.to_string());
            }
        }
        None
    }

    /// Expansion is where a small document becomes a large one. The streaming
    /// path never materialises the result, so memory stays flat -- but the time
    /// still grows exponentially, so the budget has to be charged here too.
    #[test]
    fn the_alias_expansion_budget_is_enforced() {
        let mut config = ParserConfig::default();
        config.limits.max_alias_expansions = 1;

        let input = "a: &x 1\nb: *x\nc: *x\n";
        assert!(
            drive_err(input, config.clone()).is_some_and(|e| e.contains("alias expansion")),
            "a second expansion must exceed a budget of one"
        );

        // The budget is a ceiling, not a trap: one expansion is still allowed.
        assert_eq!(drive_err("a: &x 1\nb: *x\n", config), None);
    }

    /// A separate budget from the expansion count: few aliases can still name
    /// large spans, which is the amplification the count alone would not catch.
    #[test]
    fn the_total_expanded_event_budget_is_enforced() {
        let mut config = ParserConfig::default();
        config.limits.max_total_alias_nodes = 2;

        // `x` is five events (start, three scalars, end), so one alias to it
        // already exceeds a budget of two.
        let input = "a: &x [1, 2, 3]\nb: *x\n";
        assert!(
            drive_err(input, config.clone()).is_some_and(|e| e.contains("alias")),
            "a span larger than the budget must be refused"
        );

        // A one-event anchor stays under it.
        assert_eq!(drive_err("a: &x 1\nb: *x\n", config), None);
    }
}
