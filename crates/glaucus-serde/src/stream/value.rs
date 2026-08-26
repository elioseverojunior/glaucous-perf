// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A `serde::Deserializer` over the event stream, for values of any shape.

use std::collections::{HashSet, VecDeque};

use glaucus_core::error::Result as CoreResult;
use glaucus_core::parser::event::{Event, EventKind};
use glaucus_core::types::{Tag, YamlVersion};
use serde::de::{self, DeserializeSeed, Visitor};

use crate::error::Error;
use crate::stream::scalar::ScalarDeserializer;
use crate::stream::tape::Tape;

fn err(msg: impl std::fmt::Display) -> Error {
    <Error as de::Error>::custom(msg)
}

/// Somewhere events come from.
///
/// The deserialiser reads through this rather than owning a [`Tape`] directly,
/// because an anchored value is deserialised a second time by replaying buffered
/// events (#54) -- the same traversal over a different source. It also lets the
/// truncation guards below be tested against a stream that ends cleanly
/// mid-collection, which the real parser will not produce: it raises its own
/// error at end of input first.
pub(crate) trait EventSource<'de> {
    /// Yields the next event, or `None` once the source is exhausted.
    fn next_event(&mut self) -> Option<CoreResult<Event<'de>>>;
}

impl<'de> EventSource<'de> for Tape<'de> {
    fn next_event(&mut self) -> Option<CoreResult<Event<'de>>> {
        self.next()
    }
}

/// An event source over an already-recorded run of events.
///
/// Used for merged entries, which are read out of the mapping they came from and
/// replayed later, and by tests that need a stream ending exactly where they say.
pub(crate) struct SliceSource<'de> {
    events: std::vec::IntoIter<Event<'de>>,
}

impl<'de> SliceSource<'de> {
    pub(crate) fn new(events: Vec<Event<'de>>) -> Self {
        Self {
            events: events.into_iter(),
        }
    }
}

impl<'de> EventSource<'de> for SliceSource<'de> {
    fn next_event(&mut self) -> Option<CoreResult<Event<'de>>> {
        self.events.next().map(Ok)
    }
}

/// How many events form the value starting at `events[0]`.
///
/// A scalar is one event; a collection runs to its matching end.
fn value_extent(events: &[Event<'_>]) -> usize {
    let mut depth = 0usize;
    for (index, event) in events.iter().enumerate() {
        match event.kind {
            EventKind::SequenceStart { .. } | EventKind::MappingStart { .. } => depth += 1,
            EventKind::SequenceEnd | EventKind::MappingEnd => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 {
            return index + 1;
        }
    }
    events.len()
}

/// The text of a run that is a single scalar, or `None` for any other shape.
///
/// Only scalar keys take part in precedence. The tree path appends a non-scalar
/// merged key unconditionally, because two of them cannot collide by text.
fn scalar_text<'e>(events: &'e [Event<'_>]) -> Option<&'e str> {
    match events {
        [event] => match &event.kind {
            EventKind::Scalar { value, .. } => Some(value),
            _ => None,
        },
        _ => None,
    }
}

/// Splits one `<<` value into the entries it contributes, in source order.
///
/// Accepts a mapping, or a sequence of mappings. Anything else is the error the
/// tree path raises -- a merge source has to have entries to contribute.
fn split_merge_sources<'de>(
    events: &[Event<'de>],
    out: &mut VecDeque<(Vec<Event<'de>>, Vec<Event<'de>>)>,
    merge_keys: bool,
) -> Result<(), Error> {
    match events.first().map(|e| &e.kind) {
        Some(EventKind::MappingStart { .. }) => push_mapping_entries(events, out, merge_keys),
        Some(EventKind::SequenceStart { .. }) => {
            let mut rest = inner_events(events);
            while !rest.is_empty() {
                let (item, tail) = rest.split_at(value_extent(rest));
                if !is_mapping(item) {
                    return Err(merge_value_error(item));
                }
                push_mapping_entries(item, out, merge_keys)?;
                rest = tail;
            }
            Ok(())
        }
        _ => Err(merge_value_error(events)),
    }
}

/// Whether a run of events is a mapping.
fn is_mapping(events: &[Event<'_>]) -> bool {
    // Variant imported so the `matches!` fits on one line; split across lines,
    // llvm-cov scores the pattern as its own never-executed region.
    use EventKind::MappingStart;

    matches!(events.first().map(|e| &e.kind), Some(MappingStart { .. }))
}

/// The events between a collection's start and end.
fn inner_events<'e, 'de>(events: &'e [Event<'de>]) -> &'e [Event<'de>] {
    if events.len() < 2 {
        return &[];
    }
    &events[1..events.len() - 1]
}

/// Splits a mapping's events into (key, value) runs and appends them.
///
/// A `<<` inside a merge source is expanded here too. The tree path never meets
/// this case -- an alias resolves to a node whose merges are already folded --
/// but the tape replays the source's RAW events, so an inherited merge key would
/// otherwise reach the caller as a literal `<<` entry.
fn push_mapping_entries<'de>(
    events: &[Event<'de>],
    out: &mut VecDeque<(Vec<Event<'de>>, Vec<Event<'de>>)>,
    merge_keys: bool,
) -> Result<(), Error> {
    // What this source inherits ranks below what it states itself, so it is held
    // back and appended after -- the same explicit-wins rule, one level down.
    let mut inherited = VecDeque::new();
    let mut rest = inner_events(events);

    while !rest.is_empty() {
        let (key, tail) = rest.split_at(value_extent(rest));
        if tail.is_empty() {
            // A key with no value: the run is malformed. Dropping it is right --
            // there is no value to merge, and the mapping it came from was
            // already accepted by the parser.
            break;
        }
        let (value, tail) = tail.split_at(value_extent(tail));

        if merge_keys && scalar_text(key) == Some("<<") {
            split_merge_sources(value, &mut inherited, merge_keys)?;
        } else {
            out.push_back((key.to_vec(), value.to_vec()));
        }
        rest = tail;
    }

    out.extend(inherited);
    Ok(())
}

/// The error the tree path raises for a `<<` value that cannot supply entries.
fn merge_value_error(events: &[Event<'_>]) -> Error {
    let found = events.first().map_or("nothing", |e| e.kind.name());
    err(format!(
        "expected mapping or sequence of mappings for '<<' merge key, found {found}"
    ))
}

/// What the next event is, without holding a borrow on the deserialiser.
///
/// Returned by value on purpose: the caller consumes the event immediately
/// afterwards, and handing back a `&Event` would keep a shared borrow alive
/// across the `&mut` that consuming needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Peek {
    SequenceEnd,
    MappingEnd,
    /// Stream or document framing, which wraps values rather than being one.
    Framing,
    Value,
}

/// Deserialises the next value from a stream of parser events.
pub(crate) struct EventDeserializer<'a, 'de, S: EventSource<'de>> {
    source: &'a mut S,
    version: YamlVersion,
    /// Expand `<<` merge keys. Off by default: strict YAML 1.2 has no merge key.
    merge_keys: bool,
    /// One event of lookahead, so a collection can ask "is this my end?" without
    /// consuming an element it would then have to put back.
    peeked: Option<Event<'de>>,
}

impl<'a, 'de, S: EventSource<'de>> EventDeserializer<'a, 'de, S> {
    // Reachable only from tests until #57 routes `from_str` through the
    // streaming path. Scoped to the individual item rather than the module so
    // anything that becomes dead LATER is still reported.
    #[allow(dead_code)]
    pub(crate) const fn new(source: &'a mut S, version: YamlVersion, merge_keys: bool) -> Self {
        Self {
            source,
            version,
            merge_keys,
            peeked: None,
        }
    }

    /// Ensures the lookahead slot holds the next event, if the stream has one.
    fn fill(&mut self) -> Result<(), Error> {
        if self.peeked.is_none() {
            self.peeked = match self.source.next_event() {
                None => None,
                Some(Ok(event)) => Some(event),
                Some(Err(e)) => return Err(Error::core(e)),
            };
        }
        Ok(())
    }

    /// Pulls the next event, honouring the lookahead slot.
    fn take(&mut self) -> Result<Option<Event<'de>>, Error> {
        self.fill()?;
        Ok(self.peeked.take())
    }

    /// Classifies the next event without consuming it.
    fn peek(&mut self) -> Result<Option<Peek>, Error> {
        // Imported so the framing arm fits on one line: the coverage
        // instrumentation credits a match arm to the line carrying its body, so
        // alternatives wrapped onto their own lines read as never executed.
        use EventKind::{
            DocumentEnd, DocumentStart, MappingEnd, SequenceEnd, StreamEnd, StreamStart,
        };

        self.fill()?;
        Ok(self.peeked.as_ref().map(|e| match e.kind {
            SequenceEnd => Peek::SequenceEnd,
            MappingEnd => Peek::MappingEnd,
            StreamStart | StreamEnd | DocumentStart { .. } | DocumentEnd { .. } => Peek::Framing,
            _ => Peek::Value,
        }))
    }

    /// The text of the next event if it is a scalar.
    ///
    /// Needed to spot a `<<` key and to record which keys the mapping states
    /// explicitly, both of which have to happen before the key is handed to the
    /// caller's seed and consumed.
    fn peek_scalar_text(&mut self) -> Result<Option<&str>, Error> {
        self.fill()?;
        Ok(self.peeked.as_ref().and_then(|e| match &e.kind {
            EventKind::Scalar { value, .. } => Some(&**value),
            _ => None,
        }))
    }

    /// Reads the complete next value as a run of events.
    ///
    /// A merge source is read here rather than deserialised in place because its
    /// entries are emitted at the END of the mapping, once every explicit key is
    /// known -- which is what makes explicit-wins fall out rather than be checked.
    fn capture_value(&mut self) -> Result<Vec<Event<'de>>, Error> {
        let mut out = Vec::new();
        let mut depth = 0usize;
        loop {
            let Some(event) = self.take()? else {
                return Err(err("unexpected end of input: merge value is incomplete"));
            };
            match event.kind {
                EventKind::SequenceStart { .. } | EventKind::MappingStart { .. } => depth += 1,
                EventKind::SequenceEnd | EventKind::MappingEnd => {
                    depth = depth.saturating_sub(1);
                }
                _ => {}
            }
            out.push(event);
            if depth == 0 {
                return Ok(out);
            }
        }
    }

    /// Skips the stream and document framing the parser wraps a value in.
    #[allow(dead_code)]
    pub(crate) fn skip_framing(&mut self) -> Result<(), Error> {
        while self.peek()? == Some(Peek::Framing) {
            self.take()?;
        }
        Ok(())
    }
}

impl<'de, S: EventSource<'de>> de::Deserializer<'de> for &mut EventDeserializer<'_, 'de, S> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        let Some(event) = self.take()? else {
            return Err(err("unexpected end of input: expected a value"));
        };

        let span = event.span;
        match event.kind {
            // Kept on one line, as in `Composer::compose_node_from_event`: the
            // coverage instrumentation does not credit the opening line of a
            // multi-line match pattern even though the arm executes.
            #[rustfmt::skip]
            EventKind::Scalar { value, style, anchor: _, tag } => {
                let tag = tag.map(|(handle, suffix)| Tag::resolve(handle, suffix, span));
                ScalarDeserializer::new(value, style, tag, self.version).deserialize_any(visitor)
            }
            EventKind::SequenceStart { .. } => visitor.visit_seq(SeqAccess { de: self }),
            EventKind::MappingStart { .. } => visitor.visit_map(MapAccess::new(self)),
            other => Err(err(format!("expected a value, found {}", other.name()))),
        }
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map struct enum identifier ignored_any
    }
}

/// Drives a sequence until its matching `SequenceEnd`.
struct SeqAccess<'a, 'b, 'de, S: EventSource<'de>> {
    de: &'a mut EventDeserializer<'b, 'de, S>,
}

impl<'de, S: EventSource<'de>> de::SeqAccess<'de> for SeqAccess<'_, '_, 'de, S> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Error> {
        match self.de.peek()? {
            // Truncation is an ERROR, not `Ok(None)`.
            //
            // `Ok(None)` is how a sequence reports that it ended normally, so
            // returning it here would hand back a SHORT collection with nothing
            // to distinguish it from a complete one -- `[1, 2` silently becoming
            // `vec![1, 2]`. Looping to wait for more input would hang instead.
            // Neither is acceptable on untrusted input.
            None => Err(err("unexpected end of input: sequence not terminated")),
            Some(Peek::SequenceEnd) => {
                self.de.take()?;
                Ok(None)
            }
            Some(Peek::MappingEnd) => Err(err(
                "mapping end inside a sequence: the event stream is malformed",
            )),
            Some(Peek::Value | Peek::Framing) => seed.deserialize(&mut *self.de).map(Some),
        }
    }
}

/// Drives a mapping until its matching `MappingEnd`, then its merged entries.
///
/// Merge keys are folded in at the END, after every explicit key is known. That
/// is what the tree path does, and it is why precedence needs no special case:
/// an explicit key is already in `seen` by the time a merged one is considered,
/// and an earlier source is already in `seen` by the time a later one is.
struct MapAccess<'a, 'b, 'de, S: EventSource<'de>> {
    de: &'a mut EventDeserializer<'b, 'de, S>,
    /// Scalar keys already emitted. Left empty when merge keys are off, so an
    /// ordinary mapping does not pay for a feature it is not using.
    seen: HashSet<String>,
    /// Entries contributed by `<<`, flattened in source order.
    merged: VecDeque<(Vec<Event<'de>>, Vec<Event<'de>>)>,
    /// Value events for the merged entry whose key was just handed out.
    pending: Option<Vec<Event<'de>>>,
    /// True once the mapping's own entries are exhausted.
    draining: bool,
}

impl<'a, 'b, 'de, S: EventSource<'de>> MapAccess<'a, 'b, 'de, S> {
    fn new(de: &'a mut EventDeserializer<'b, 'de, S>) -> Self {
        Self {
            de,
            seen: HashSet::new(),
            merged: VecDeque::new(),
            pending: None,
            draining: false,
        }
    }

    /// Hands out the next merged key that nothing has already claimed.
    fn next_merged_key<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        while let Some((key, value)) = self.merged.pop_front() {
            // A key already present loses. Explicit keys went in first and
            // earlier sources before later ones, so one `insert` decides both
            // rules. A non-scalar key has no text to collide on and is kept.
            if let Some(text) = scalar_text(&key)
                && !self.seen.insert(text.to_owned())
            {
                continue;
            }
            self.pending = Some(value);
            return self.deserialize_recorded(key, seed).map(Some);
        }
        Ok(None)
    }

    /// Deserialises a recorded run of events.
    fn deserialize_recorded<T: DeserializeSeed<'de>>(
        &self,
        events: Vec<Event<'de>>,
        seed: T,
    ) -> Result<T::Value, Error> {
        let mut source = SliceSource::new(events);
        let mut de = EventDeserializer::new(&mut source, self.de.version, self.de.merge_keys);
        seed.deserialize(&mut de)
    }
}

impl<'de, S: EventSource<'de>> de::MapAccess<'de> for MapAccess<'_, '_, 'de, S> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        loop {
            if self.draining {
                return self.next_merged_key(seed);
            }

            match self.de.peek()? {
                None => return Err(err("unexpected end of input: mapping not terminated")),
                Some(Peek::MappingEnd) => {
                    self.de.take()?;
                    if self.merged.is_empty() {
                        return Ok(None);
                    }
                    self.draining = true;
                    continue;
                }
                Some(Peek::SequenceEnd) => {
                    return Err(err(
                        "sequence end inside a mapping: the event stream is malformed",
                    ));
                }
                Some(Peek::Value | Peek::Framing) => {}
            }

            // Owned because the borrow has to end before the key is consumed.
            // Only paid for when merge keys are on, which is opt-in.
            let text = if self.de.merge_keys {
                self.de.peek_scalar_text()?.map(str::to_owned)
            } else {
                None
            };

            // `Composer::is_merge_key` asks only for the scalar's text, so a
            // quoted `"<<"` merges too. Matching the reference matters more here
            // than what the spec implies.
            if text.as_deref() == Some("<<") {
                self.de.take()?;
                let value = self.de.capture_value()?;
                split_merge_sources(&value, &mut self.merged, self.de.merge_keys)?;
                continue;
            }

            if let Some(text) = text {
                self.seen.insert(text);
            }
            return seed.deserialize(&mut *self.de).map(Some);
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
        if let Some(events) = self.pending.take() {
            return self.deserialize_recorded(events, seed);
        }

        // A key with no value is truncation, not an empty mapping: the key has
        // already been consumed, so there is no honest way to report "nothing
        // here" -- returning a default would invent a value the input never had.
        if self.de.peek()?.is_none() {
            return Err(err("unexpected end of input: mapping key has no value"));
        }
        seed.deserialize(&mut *self.de)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    use glaucus_core::types::{CollectionStyle, Position, ScalarStyle, Span};
    use serde::Deserialize;

    use super::*;

    /// Deserialises `input` by driving the real parser through a [`Tape`].
    fn from_yaml<'de, T: Deserialize<'de>>(input: &'de str) -> Result<T, Error> {
        let mut tape = Tape::new(input);
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2, false);
        de.skip_framing()?;
        T::deserialize(&mut de)
    }

    // --- real parser -------------------------------------------------------

    #[test]
    fn flow_sequence_becomes_a_vec() {
        assert_eq!(from_yaml::<Vec<i64>>("[1, 2, 3]").unwrap(), vec![1, 2, 3]);
    }

    #[test]
    fn block_sequence_becomes_a_vec() {
        assert_eq!(from_yaml::<Vec<i64>>("- 1\n- 2\n").unwrap(), vec![1, 2]);
    }

    #[test]
    fn empty_flow_collections_round_trip() {
        assert_eq!(from_yaml::<Vec<i64>>("[]").unwrap(), Vec::<i64>::new());
        assert!(from_yaml::<BTreeMap<String, i64>>("{}").unwrap().is_empty());
    }

    #[test]
    fn block_mapping_becomes_a_map() {
        let got: BTreeMap<String, i64> = from_yaml("a: 1\nb: 2\n").unwrap();
        assert_eq!(got, BTreeMap::from([("a".into(), 1), ("b".into(), 2)]));
    }

    #[test]
    fn mapping_becomes_a_struct() {
        #[derive(Debug, Deserialize, PartialEq)]
        struct Config {
            name: String,
            retries: u8,
        }

        assert_eq!(
            from_yaml::<Config>("name: glaucus\nretries: 3\n").unwrap(),
            Config {
                name: "glaucus".into(),
                retries: 3,
            }
        );
    }

    #[test]
    fn collections_nest() {
        let got: BTreeMap<String, Vec<Vec<i64>>> = from_yaml("a: [[1, 2], [3]]\n").unwrap();
        assert_eq!(got["a"], vec![vec![1, 2], vec![3]]);
    }

    #[test]
    fn scalars_inside_collections_keep_core_schema_resolution() {
        // The sequence path must reach the same resolver the scalar path uses;
        // `no` is a string under 1.2 and `0x1F` is 31, not the text "0x1F".
        // A heterogeneous tuple lets each element assert its own resolved type.
        let got: (String, i64, Option<i64>, f64) = from_yaml("[no, 0x1F, ~, 1.5]").unwrap();
        assert_eq!(got, ("no".to_string(), 31, None, 1.5));
    }

    #[test]
    fn sequence_elements_borrow_from_the_input() {
        // Proves the collection path forwards to `visit_borrowed_str`: a `&str`
        // target cannot be satisfied from a transient buffer.
        let got: Vec<&str> = from_yaml("[alpha, beta]").unwrap();
        assert_eq!(got, vec!["alpha", "beta"]);
    }

    #[test]
    fn parser_level_truncation_surfaces_as_an_error() {
        // The parser raises at end of input before the stream ends, so this
        // never reaches the guards below -- but it must still be an error, and
        // must never yield a short `vec![1, 2]`.
        let got = from_yaml::<Vec<i64>>("[1, 2");
        assert!(got.is_err(), "truncated flow sequence parsed as {got:?}");
    }

    // --- injected streams --------------------------------------------------

    fn at(kind: EventKind<'_>) -> Event<'_> {
        Event {
            kind,
            span: Span::point(Position::start()),
        }
    }

    fn scalar(value: &str) -> Event<'_> {
        at(EventKind::Scalar {
            value: Cow::Borrowed(value),
            style: ScalarStyle::Plain,
            anchor: None,
            tag: None,
        })
    }

    fn seq_start<'de>() -> Event<'de> {
        at(EventKind::SequenceStart {
            anchor: None,
            tag: None,
            style: CollectionStyle::Flow,
        })
    }

    fn map_start<'de>() -> Event<'de> {
        at(EventKind::MappingStart {
            anchor: None,
            tag: None,
            style: CollectionStyle::Flow,
        })
    }

    /// Deserialises `T` from injected events.
    fn from_events<'de, T: Deserialize<'de>>(events: Vec<Event<'de>>) -> Result<T, Error> {
        let mut source = SliceSource::new(events);
        let mut de = EventDeserializer::new(&mut source, YamlVersion::V1_2, false);
        T::deserialize(&mut de)
    }

    #[test]
    fn injected_stream_deserializes_like_the_parser() {
        // Guards the stub itself: if this failed, the truncation tests below
        // would prove nothing about the real path.
        let got: Vec<i64> =
            from_events(vec![seq_start(), scalar("1"), at(EventKind::SequenceEnd)]).unwrap();
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn sequence_that_ends_early_is_an_error_not_a_short_vec() {
        let got = from_events::<Vec<i64>>(vec![seq_start(), scalar("1"), scalar("2")]);
        let msg = got
            .expect_err("truncated sequence deserialized")
            .to_string();
        assert!(msg.contains("sequence not terminated"), "{msg}");
    }

    #[test]
    fn mapping_that_ends_early_is_an_error() {
        let got = from_events::<BTreeMap<String, i64>>(vec![map_start(), scalar("a"), scalar("1")]);
        let msg = got.expect_err("truncated mapping deserialized").to_string();
        assert!(msg.contains("mapping not terminated"), "{msg}");
    }

    #[test]
    fn mapping_key_without_a_value_is_an_error() {
        let got = from_events::<BTreeMap<String, i64>>(vec![map_start(), scalar("a")]);
        let msg = got.expect_err("dangling key deserialized").to_string();
        assert!(msg.contains("mapping key has no value"), "{msg}");
    }

    #[test]
    fn empty_stream_where_a_value_is_expected_is_an_error() {
        let msg = from_events::<i64>(vec![])
            .expect_err("empty stream deserialized")
            .to_string();
        assert!(msg.contains("expected a value"), "{msg}");
    }

    #[test]
    fn mismatched_end_events_are_rejected() {
        let msg = from_events::<Vec<i64>>(vec![seq_start(), at(EventKind::MappingEnd)])
            .expect_err("mapping end closed a sequence")
            .to_string();
        assert!(msg.contains("mapping end inside a sequence"), "{msg}");

        let msg =
            from_events::<BTreeMap<String, i64>>(vec![map_start(), at(EventKind::SequenceEnd)])
                .expect_err("sequence end closed a mapping")
                .to_string();
        assert!(msg.contains("sequence end inside a mapping"), "{msg}");
    }

    #[test]
    fn a_non_value_event_where_a_value_belongs_is_reported_by_name() {
        let msg = from_events::<i64>(vec![at(EventKind::StreamEnd)])
            .expect_err("stream-end deserialized as a value")
            .to_string();
        assert!(msg.contains("expected a value, found stream-end"), "{msg}");
    }

    #[test]
    fn skip_framing_stops_at_the_first_real_value() {
        let mut tape = Tape::new("- 1\n");
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2, false);
        de.skip_framing().unwrap();
        assert_eq!(de.peek().unwrap(), Some(Peek::Value));
    }

    #[test]
    fn skip_framing_on_an_empty_document_reaches_the_end() {
        let mut tape = Tape::new("");
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2, false);
        de.skip_framing().unwrap();
        assert_eq!(de.peek().unwrap(), None);
    }

    #[test]
    fn skip_framing_steps_over_every_framing_event() {
        // Injected rather than parsed so all four framing kinds appear in one
        // stream: a real document does not necessarily emit each of them, and a
        // framing kind that `skip_framing` failed to recognise would otherwise
        // surface later as a baffling "expected a value, found document-end".
        let mut source = SliceSource::new(vec![
            at(EventKind::StreamStart),
            at(EventKind::DocumentStart {
                explicit: true,
                version: YamlVersion::V1_2,
            }),
            scalar("7"),
            at(EventKind::DocumentEnd { explicit: true }),
            at(EventKind::StreamEnd),
        ]);
        let mut de = EventDeserializer::new(&mut source, YamlVersion::V1_2, false);

        de.skip_framing().unwrap();
        assert_eq!(i64::deserialize(&mut de).unwrap(), 7);

        // The trailing framing is skipped just as the leading framing was.
        de.skip_framing().unwrap();
        assert_eq!(de.peek().unwrap(), None);
    }

    // --- anchors and aliases (#54) -----------------------------------------

    #[test]
    fn an_alias_resolves_to_its_anchor() {
        let got: BTreeMap<String, Vec<i64>> = from_yaml("a: &x [1, 2]\nb: *x\n").unwrap();
        assert_eq!(got["a"], vec![1, 2]);
        assert_eq!(got["b"], got["a"]);
    }

    #[test]
    fn an_alias_to_a_scalar_resolves() {
        let got: BTreeMap<String, i64> = from_yaml("a: &x 7\nb: *x\n").unwrap();
        assert_eq!(got["b"], 7);
    }

    #[test]
    fn nested_anchors_resolve_independently_and_when_nested() {
        // `y` sits inside `x`. Replaying `x` must reproduce `y`'s events rather
        // than a marker, and `y` must still resolve on its own afterwards.
        #[derive(Debug, Deserialize, PartialEq)]
        struct Inner {
            p: Vec<i64>,
            q: Vec<i64>,
        }

        let got: BTreeMap<String, Inner> =
            from_yaml("a: &x {p: &y [1, 2], q: *y}\nb: *x\n").unwrap();
        assert_eq!(got["a"].p, vec![1, 2]);
        assert_eq!(got["a"].q, vec![1, 2]);
        assert_eq!(got["b"], got["a"]);
    }

    #[test]
    fn an_alias_inside_an_anchored_value_survives_replay() {
        // `z`'s recorded span contains an alias event. Replaying `z` must expand
        // that alias again rather than skipping it -- the case that breaks if
        // replayed events are recorded instead of the alias itself.
        // A struct, not a map: the three values have different shapes, and a
        // uniform map value type would fail on `a` before reaching the alias.
        #[derive(Debug, Deserialize, PartialEq)]
        struct Doc {
            a: Vec<i64>,
            z: Vec<Vec<i64>>,
            c: Vec<Vec<i64>>,
        }

        let got: Doc = from_yaml("a: &x [1]\nz: &z [*x, *x]\nc: *z\n").unwrap();
        assert_eq!(got.a, vec![1]);
        assert_eq!(got.z, vec![vec![1], vec![1]]);
        assert_eq!(got.c, got.z);
    }

    #[test]
    fn an_anchor_aliased_repeatedly_resolves_every_time() {
        let got: BTreeMap<String, Vec<i64>> =
            from_yaml("a: &x [1]\nb: *x\nc: *x\nd: *x\n").unwrap();
        for key in ["b", "c", "d"] {
            assert_eq!(got[key], vec![1], "{key}");
        }
    }

    #[test]
    fn an_undefined_alias_is_an_error() {
        let msg = from_yaml::<BTreeMap<String, i64>>("a: *nope\n")
            .expect_err("undefined alias deserialized")
            .to_string();
        assert!(msg.contains("undefined alias"), "{msg}");
    }

    #[test]
    fn an_anchor_cannot_alias_itself() {
        // At `*x` the scope for `x` is still open, so `x` is not yet a defined
        // anchor. That is what stops a self-referential document from looping.
        let got = from_yaml::<BTreeMap<String, Vec<i64>>>("a: &x [*x]\n");
        assert!(got.is_err(), "self-referential alias resolved: {got:?}");
    }

    /// A float compared so that `NaN` equals `NaN`.
    ///
    /// IEEE 754 says `NaN != NaN`, so a derived comparison reports `.nan` as a
    /// disagreement on every run -- the harness would fail permanently on a document
    /// both engines handled identically, and a harness that always fails detects
    /// nothing. Equality here asks the question the harness actually means: did the
    /// two engines produce the same value?
    #[derive(Debug, Deserialize)]
    #[serde(transparent)]
    struct F64(f64);

    impl PartialEq for F64 {
        fn eq(&self, other: &Self) -> bool {
            // Bit equality would also split +0.0 from -0.0, which the engines are
            // not expected to distinguish, so `==` still decides the ordinary case.
            (self.0.is_nan() && other.0.is_nan()) || self.0 == other.0
        }
    }

    /// A span-free, style-free view of a document, for comparing two engines.
    ///
    /// `Value` cannot do this job. Its `PartialEq` compares `span` and `style`,
    /// and neither survives serde's data model -- the streaming path builds
    /// values from `Visitor` calls that carry no source position, so every
    /// document would report a difference and the real ones would be buried.
    /// What must agree across engines is the structure and the scalar text.
    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(untagged)]
    enum Shape {
        // Order matters: serde tries these top to bottom. Collections first so a
        // sequence is never mistaken for a string, and Int before Float so a
        // whole number does not silently become 1.0 on one path only.
        Seq(Vec<Self>),
        Map(BTreeMap<String, Self>),
        Bool(bool),
        Int(i64),
        Float(F64),
        Str(String),
        Null,
    }

    /// The acceptance criterion of #54: every anchor shape must produce the same
    /// value on both engines.
    ///
    /// The differential harness in `tests/differential.rs` cannot do this yet --
    /// it is a separate crate and the streaming types are `pub(crate)` until #57
    /// routes `from_str`. Comparing here keeps the guarantee tested rather than
    /// deferred to an integration task, which is what #54 asks for.
    #[test]
    fn anchors_agree_with_the_tree_path() {
        const CASES: &[&str] = &[
            "a: &x 1\nb: *x\n",
            "a: &x {p: 1, q: 2}\nb: *x\n",
            "a: &x [1, 2, 3]\nb: *x\n",
            "a: &outer\n  inner: &in [1, 2]\n  other: *in\nb: *outer\n",
            "a: &x [1, 2]\nb: *x\nc: *x\nd: *x\ne: *x\n",
            "a: &x [1]\nz: &z [*x, *x]\nc: *z\n",
            "a: &x {p: &y [1, 2], q: *y}\nb: *x\n",
            // An anchor that is never aliased must not change the result.
            "a: &unused [1, 2]\nb: 3\n",
            // A redefined anchor: the later definition wins.
            "a: &x 1\nb: &x 2\nc: *x\n",
        ];

        let mut disagreements = Vec::new();
        for yaml in CASES {
            let tree = crate::from_str::<Shape>(yaml).map_err(|e| e.to_string());
            let streaming = from_yaml::<Shape>(yaml).map_err(|e| e.to_string());

            match (&tree, &streaming) {
                (Ok(t), Ok(s)) if t == s => {}
                _ => disagreements.push(format!(
                    "{yaml:?}\n  tree      = {tree:?}\n  streaming = {streaming:?}"
                )),
            }
        }

        assert!(
            disagreements.is_empty(),
            "{} of {} anchor cases disagree:\n\n{}",
            disagreements.len(),
            CASES.len(),
            disagreements.join("\n\n")
        );
    }

    #[test]
    fn a_self_referential_anchor_is_an_undefined_alias() {
        // Stronger than "it errors": before #54 this failed with "expected a
        // value, found alias", which would have passed a mere is_err check while
        // proving nothing about loop safety.
        let msg = from_yaml::<BTreeMap<String, Vec<i64>>>("a: &x [*x]\n")
            .expect_err("self-referential alias resolved")
            .to_string();
        assert!(msg.contains("undefined alias"), "{msg}");
    }

    // --- merge keys (#55) ---------------------------------------------------

    /// Deserialises with `<<` merge-key expansion enabled.
    fn from_yaml_merged<'de, T: Deserialize<'de>>(input: &'de str) -> Result<T, Error> {
        let mut tape = Tape::new(input);
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2, true);
        de.skip_framing()?;
        T::deserialize(&mut de)
    }

    /// Only the merged mapping matters; the anchor definitions are ignored.
    #[derive(Debug, Deserialize, PartialEq)]
    struct Merged {
        m: BTreeMap<String, i64>,
    }

    #[test]
    fn a_simple_merge_supplies_defaults() {
        let got: Merged = from_yaml_merged("d: &d {a: 1, b: 2}\nm:\n  <<: *d\n").unwrap();
        assert_eq!(got.m, BTreeMap::from([("a".into(), 1), ("b".into(), 2)]));
    }

    #[test]
    fn an_explicit_key_beats_a_merged_one() {
        let got: Merged = from_yaml_merged("d: &d {a: 1, b: 2}\nm:\n  <<: *d\n  a: 9\n").unwrap();
        assert_eq!(got.m["a"], 9, "the explicit key must win");
        assert_eq!(got.m["b"], 2);
    }

    #[test]
    fn an_explicit_key_wins_even_when_written_before_the_merge() {
        // The tree path folds merges in AFTER the whole mapping is read, so the
        // position of `<<` never decides precedence.
        let got: Merged = from_yaml_merged("d: &d {a: 1}\nm:\n  a: 9\n  <<: *d\n").unwrap();
        assert_eq!(got.m["a"], 9);
    }

    #[test]
    fn in_a_merge_sequence_the_earlier_source_wins() {
        let got: Merged =
            from_yaml_merged("x: &x {a: 1}\ny: &y {a: 8, b: 2}\nm:\n  <<: [*x, *y]\n").unwrap();
        assert_eq!(got.m["a"], 1, "the earlier source must win");
        assert_eq!(got.m["b"], 2);
    }

    #[test]
    fn repeated_merge_keys_are_all_applied_earlier_first() {
        let got: Merged =
            from_yaml_merged("x: &x {a: 1}\ny: &y {a: 8, b: 2}\nm:\n  <<: *x\n  <<: *y\n").unwrap();
        assert_eq!(got.m["a"], 1);
        assert_eq!(got.m["b"], 2);
    }

    #[test]
    fn a_quoted_merge_key_still_merges() {
        // `Composer::is_merge_key` asks only for the scalar text, so quoting does
        // not opt out. Matching that matters more than what the spec implies.
        let got: Merged = from_yaml_merged("d: &d {a: 1}\nm:\n  \"<<\": *d\n").unwrap();
        assert_eq!(got.m["a"], 1);
    }

    #[test]
    fn an_inline_mapping_merges_without_an_anchor() {
        let got: Merged = from_yaml_merged("m:\n  z: 0\n  <<: {a: 1}\n").unwrap();
        assert_eq!(got.m, BTreeMap::from([("z".into(), 0), ("a".into(), 1)]));
    }

    #[test]
    fn a_scalar_merge_value_is_an_error() {
        let msg = from_yaml_merged::<Merged>("m:\n  <<: 5\n")
            .expect_err("a scalar merge value was accepted")
            .to_string();
        assert!(msg.contains("merge key"), "{msg}");
    }

    #[test]
    fn a_non_mapping_in_a_merge_sequence_is_an_error() {
        let msg = from_yaml_merged::<Merged>("x: &x {a: 1}\nm:\n  <<: [*x, 5]\n")
            .expect_err("a scalar inside a merge sequence was accepted")
            .to_string();
        assert!(msg.contains("merge key"), "{msg}");
    }

    #[test]
    fn merge_keys_stay_literal_when_the_feature_is_off() {
        // Strict YAML 1.2 has no merge key, which is why this is opt-in. With it
        // off, `<<` is an ordinary key and must reach the target as one.
        let got: Shape = from_yaml("m:\n  <<: {a: 1}\n").unwrap();
        let expected = Shape::Map(BTreeMap::from([(
            "m".to_string(),
            Shape::Map(BTreeMap::from([(
                "<<".to_string(),
                Shape::Map(BTreeMap::from([("a".to_string(), Shape::Int(1))])),
            )])),
        )]));
        assert_eq!(got, expected, "`<<` must stay an ordinary key");
    }

    #[test]
    fn a_source_key_beats_the_one_it_inherits() {
        // `o` states `a: 2` and inherits `a: 1` from `i`. What a mapping says
        // itself outranks what it merged in, at every level -- so `m`, which
        // merges `o`, sees 2. Without this the ordering of a merge source's own
        // entries against its inherited ones is unobservable and can silently
        // invert.
        let got: Merged =
            from_yaml_merged("i: &i {a: 1}\no: &o {<<: *i, a: 2}\nm:\n  <<: *o\n").unwrap();
        assert_eq!(got.m["a"], 2);

        // ...and the target still overrides both.
        let got: Merged =
            from_yaml_merged("i: &i {a: 1}\no: &o {<<: *i, a: 2}\nm:\n  <<: *o\n  a: 3\n").unwrap();
        assert_eq!(got.m["a"], 3);
    }

    /// The acceptance criterion of #55: precedence must match the tree path.
    #[test]
    fn merges_agree_with_the_tree_path() {
        const CASES: &[&str] = &[
            "d: &d {a: 1, b: 2}\nm:\n  <<: *d\n",
            "d: &d {a: 1, b: 2}\nm:\n  <<: *d\n  a: 9\n",
            "x: &x {a: 1}\ny: &y {a: 8, b: 2}\nm:\n  <<: [*x, *y]\n",
            "m:\n  z: 0\n  <<: {a: 1}\n  y: 9\n",
            "d: &d {a: 1}\nm:\n  \"<<\": *d\n",
            "x: &x {a: 1}\ny: &y {b: 2}\nm:\n  <<: *x\n  <<: *y\n",
            "d: &d {a: 1}\nm: {<<: *d, b: 2}\n",
            // A merged value that is itself a mapping with a merge key.
            "i: &i {p: 1}\no: &o {<<: *i, q: 2}\nm:\n  <<: *o\n",
            // ...where the source's OWN key collides with what it inherits.
            "i: &i {a: 1}\no: &o {<<: *i, a: 2}\nm:\n  <<: *o\n",
            "i: &i {a: 1}\no: &o {<<: *i, a: 2}\nm:\n  <<: *o\n  a: 3\n",
            "i: &i {a: 1, b: 1}\no: &o {<<: *i, b: 2, c: 2}\nm:\n  <<: *o\n  c: 3\n",
        ];

        let config = glaucus_core::error::ParserConfig {
            merge_keys: true,
            ..Default::default()
        };

        let mut disagreements = Vec::new();
        for yaml in CASES {
            let tree =
                crate::from_str_with::<Shape>(yaml, config.clone()).map_err(|e| e.to_string());
            let streaming = from_yaml_merged::<Shape>(yaml).map_err(|e| e.to_string());

            match (&tree, &streaming) {
                (Ok(t), Ok(s)) if t == s => {}
                _ => disagreements.push(format!(
                    "{yaml:?}\n  tree      = {tree:?}\n  streaming = {streaming:?}"
                )),
            }
        }

        assert!(
            disagreements.is_empty(),
            "{} of {} merge cases disagree:\n\n{}",
            disagreements.len(),
            CASES.len(),
            disagreements.join("\n\n")
        );
    }

    // --- merge-source splitting, directly ----------------------------------
    //
    // These helpers are reached through the deserialiser on well-formed input
    // only, so their defensive paths cannot be provoked from a document. Calling
    // them directly is what makes those paths tested rather than merely present.

    #[test]
    fn value_extent_measures_a_scalar_and_a_collection() {
        assert_eq!(value_extent(&[scalar("a"), scalar("b")]), 1);
        assert_eq!(
            value_extent(&[seq_start(), scalar("1"), at(EventKind::SequenceEnd)]),
            3
        );
    }

    #[test]
    fn value_extent_of_an_unbalanced_run_is_the_whole_run() {
        // No matching end, so there is no shorter honest answer than "all of it".
        // Returning a partial extent would split one value across two entries.
        assert_eq!(value_extent(&[seq_start(), scalar("1")]), 2);
    }

    #[test]
    fn scalar_text_accepts_only_a_lone_scalar() {
        assert_eq!(scalar_text(&[scalar("k")]), Some("k"));
        // A collection key: more than one event, so it has no text to collide on.
        assert_eq!(
            scalar_text(&[seq_start(), scalar("1"), at(EventKind::SequenceEnd)]),
            None
        );
        // A single event that is not a scalar.
        assert_eq!(scalar_text(&[seq_start()]), None);
    }

    #[test]
    fn inner_events_of_a_run_too_short_to_have_any_is_empty() {
        assert!(inner_events(&[]).is_empty());
        assert!(inner_events(&[seq_start()]).is_empty());
        assert!(inner_events(&[map_start(), at(EventKind::MappingEnd)]).is_empty());
    }

    #[test]
    fn a_dangling_key_in_a_merge_source_is_dropped() {
        // A key with no value cannot contribute an entry. The parser does not
        // produce this, but the helper must not pair the key with whatever
        // follows the mapping either.
        let mut out = VecDeque::new();
        let events = [map_start(), scalar("a"), at(EventKind::MappingEnd)];
        push_mapping_entries(&events, &mut out, false).unwrap();
        assert!(out.is_empty(), "got {out:?}");
    }

    #[test]
    fn a_merge_value_that_ends_mid_stream_is_an_error() {
        // Reachable only from an injected stream: the parser raises its own
        // error at end of input before a value can simply stop.
        let mut source = SliceSource::new(vec![map_start(), scalar("a")]);
        let mut de = EventDeserializer::new(&mut source, YamlVersion::V1_2, true);
        let msg = de
            .capture_value()
            .expect_err("an unterminated merge value was captured")
            .to_string();
        assert!(msg.contains("merge value is incomplete"), "{msg}");
    }

    /// Mapping entries with keys of any shape, in document order.
    ///
    /// `Shape` cannot express these: its `Map` is keyed by `String` because that
    /// is what an ordinary document needs, and a `BTreeMap` would also discard
    /// the order that merge folding is supposed to produce.
    #[derive(Debug, PartialEq)]
    struct Entries(Vec<(Shape, Shape)>);

    impl<'de> Deserialize<'de> for Entries {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Visit;

            impl<'de> Visitor<'de> for Visit {
                type Value = Entries;

                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a mapping")
                }

                fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Entries, A::Error> {
                    let mut out = Vec::new();
                    while let Some(entry) = map.next_entry()? {
                        out.push(entry);
                    }
                    Ok(Entries(out))
                }
            }

            d.deserialize_map(Visit)
        }
    }

    /// Deserialises `yaml` on both engines with merge keys on.
    fn both_paths(yaml: &str) -> (Result<Entries, String>, Result<Entries, String>) {
        let config = glaucus_core::error::ParserConfig {
            merge_keys: true,
            ..Default::default()
        };
        (
            crate::from_str_with::<Entries>(yaml, config).map_err(|e| e.to_string()),
            from_yaml_merged::<Entries>(yaml).map_err(|e| e.to_string()),
        )
    }

    #[test]
    fn a_non_scalar_merged_key_is_kept_without_deduplication() {
        // Two collection keys cannot collide by text, so the tree path appends
        // them unconditionally. Streaming must not drop them.
        let (tree, streaming) = both_paths("<<: {? [1, 2]\n     : v}\n");
        assert_eq!(streaming, tree);
        assert!(tree.is_ok(), "{tree:?}");
    }

    #[test]
    fn a_collection_key_is_not_mistaken_for_a_merge_key() {
        // `peek_scalar_text` yields None for a non-scalar key. That must read as
        // "no text to compare", not as an unchecked key.
        let (tree, streaming) = both_paths("? [1, 2]\n: v\n");
        assert_eq!(streaming, tree);
        assert!(tree.is_ok(), "{tree:?}");
    }

    #[test]
    fn merged_entries_follow_the_explicit_ones_in_order() {
        // Order is part of matching the tree path, and every other merge test
        // compares through a BTreeMap, which throws it away.
        let (tree, streaming) = both_paths("z: 0\n<<: {a: 1}\ny: 9\n");
        assert_eq!(streaming, tree);

        let keys: Vec<Shape> = streaming.unwrap().0.into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            keys,
            vec![
                Shape::Str("z".into()),
                Shape::Str("y".into()),
                Shape::Str("a".into()),
            ],
            "explicit keys keep document order and merged ones come after"
        );
    }

    // --- resource limits (#56) ---------------------------------------------

    /// Deserialises through the streaming path with caller-supplied limits.
    fn from_yaml_with<'de, T: Deserialize<'de>>(
        input: &'de str,
        config: glaucus_core::error::ParserConfig,
    ) -> Result<T, Error> {
        let merge_keys = config.merge_keys;
        let mut tape = Tape::with_config(input, config);
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2, merge_keys);
        de.skip_framing()?;
        T::deserialize(&mut de)
    }

    /// Both engines must reach the same verdict when a budget is tight.
    ///
    /// Only accept-versus-reject is compared, as in the differential harness:
    /// two engines can refuse a document for the same reason and word it
    /// differently, and treating that as a difference would bury the real signal.
    #[test]
    fn limits_reach_the_same_verdict_on_both_paths() {
        use glaucus_core::error::ParserConfig;
        use glaucus_core::limits::ResourceLimits;

        let tight = |limits: ResourceLimits| ParserConfig {
            limits,
            ..Default::default()
        };

        let cases: &[(&str, ParserConfig)] = &[
            (
                "a: &x 1\nb: &y 2\n",
                tight(ResourceLimits {
                    max_anchors: 1,
                    ..Default::default()
                }),
            ),
            (
                "a: &x 1\nb: &x 2\n",
                tight(ResourceLimits {
                    max_anchors: 1,
                    ..Default::default()
                }),
            ),
            (
                "a: &toolong 1\n",
                tight(ResourceLimits {
                    max_anchor_name_length: 2,
                    ..Default::default()
                }),
            ),
            (
                "a: &ok 1\n",
                tight(ResourceLimits {
                    max_anchor_name_length: 2,
                    ..Default::default()
                }),
            ),
            (
                "a: abcdefghij\n",
                tight(ResourceLimits {
                    max_scalar_length: 3,
                    ..Default::default()
                }),
            ),
            (
                "a: ab\n",
                tight(ResourceLimits {
                    max_scalar_length: 3,
                    ..Default::default()
                }),
            ),
            (
                "a: [[[[1]]]]\n",
                tight(ResourceLimits {
                    max_depth: 3,
                    ..Default::default()
                }),
            ),
            (
                "a: &x 1\nb: *x\nc: *x\n",
                tight(ResourceLimits {
                    max_alias_expansions: 1,
                    ..Default::default()
                }),
            ),
        ];

        let mut disagreements = Vec::new();
        for (yaml, config) in cases {
            let tree = crate::from_str_with::<Shape>(yaml, config.clone()).is_ok();
            let streaming = from_yaml_with::<Shape>(yaml, config.clone()).is_ok();
            if tree != streaming {
                disagreements.push(format!(
                    "{yaml:?}\n  tree accepted = {tree}, streaming accepted = {streaming}"
                ));
            }
        }

        assert!(
            disagreements.is_empty(),
            "{} of {} limit cases disagree:\n\n{}",
            disagreements.len(),
            cases.len(),
            disagreements.join("\n\n")
        );
    }

    /// Two DELIBERATE divergences, in the same direction: streaming is stricter.
    ///
    /// Both come from the same fact -- the tree path materialises an anchor once
    /// and clones the result, while streaming re-walks the events every time.
    /// Streaming therefore sees work the tree path never performs, and bounds it.
    /// Pinned so each difference stays a decision rather than a surprise, and so
    /// nothing "fixes" streaming to match without weighing what is lost.
    #[test]
    fn streaming_is_stricter_than_the_tree_path_in_two_known_places() {
        use glaucus_core::error::ParserConfig;
        use glaucus_core::limits::ResourceLimits;

        // 1. Depth introduced by expansion.
        //
        // With `max_depth: 3` the tree path refuses `[[[[1]]]]` but accepts this,
        // materialising a six-deep tree: the parser bounded what it READ, and
        // expansion happens afterwards. Streaming refuses it, because the
        // expanded stream is what recurses through serde -- an unbounded one is
        // the stack overflow #33 turned into a catchable error, and this is the
        // path meant to become the default.
        let deep = ParserConfig {
            limits: ResourceLimits {
                max_depth: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let yaml = "a: &a [[1]]\nb: [[*a]]\n";
        assert!(
            crate::from_str_with::<Shape>(yaml, deep.clone()).is_ok(),
            "the tree path is expected to accept this today"
        );
        assert!(
            from_yaml_with::<Shape>(yaml, deep).is_err(),
            "streaming must bound the depth an alias introduces"
        );

        // 2. Where the billion-laughs line falls.
        //
        // The tree path charges each alias the size of the anchor it names, once
        // -- 74,718 nodes here, inside the 100,000 cap, so it is accepted.
        // Streaming charges every re-expansion, and the nine aliases at each
        // level re-expand the level below: 7,380 expansions against a cap of
        // 1,024. Both are real protections; they draw the line in different
        // places, and one more level would trip the tree path too.
        let bomb = concat!(
            "a: &a [x,x,x,x,x,x,x,x,x]\n",
            "b: &b [*a,*a,*a,*a,*a,*a,*a,*a,*a]\n",
            "c: &c [*b,*b,*b,*b,*b,*b,*b,*b,*b]\n",
            "d: &d [*c,*c,*c,*c,*c,*c,*c,*c,*c]\n",
            "e: [*d,*d,*d,*d,*d,*d,*d,*d,*d]\n",
        );
        assert!(
            crate::from_str_with::<Shape>(bomb, ParserConfig::default()).is_ok(),
            "the tree path is expected to accept this today"
        );
        assert!(
            from_yaml_with::<Shape>(bomb, ParserConfig::default()).is_err(),
            "streaming must refuse a billion-laughs document"
        );
    }
}
