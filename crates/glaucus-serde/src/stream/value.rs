// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A `serde::Deserializer` over the event stream, for values of any shape.

// Constructed only by tests until #57 routes `from_str` through the streaming
// path. Scoped to this module and tied to that issue -- delete it when #57
// lands and the compiler will confirm it is no longer needed.
#![allow(dead_code)]

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
    /// One event of lookahead, so a collection can ask "is this my end?" without
    /// consuming an element it would then have to put back.
    peeked: Option<Event<'de>>,
}

impl<'a, 'de, S: EventSource<'de>> EventDeserializer<'a, 'de, S> {
    pub(crate) const fn new(source: &'a mut S, version: YamlVersion) -> Self {
        Self {
            source,
            version,
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

    /// Skips the stream and document framing the parser wraps a value in.
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
            EventKind::MappingStart { .. } => visitor.visit_map(MapAccess { de: self }),
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

/// Drives a mapping until its matching `MappingEnd`.
struct MapAccess<'a, 'b, 'de, S: EventSource<'de>> {
    de: &'a mut EventDeserializer<'b, 'de, S>,
}

impl<'de, S: EventSource<'de>> de::MapAccess<'de> for MapAccess<'_, '_, 'de, S> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Error> {
        match self.de.peek()? {
            None => Err(err("unexpected end of input: mapping not terminated")),
            Some(Peek::MappingEnd) => {
                self.de.take()?;
                Ok(None)
            }
            Some(Peek::SequenceEnd) => Err(err(
                "sequence end inside a mapping: the event stream is malformed",
            )),
            Some(Peek::Value | Peek::Framing) => seed.deserialize(&mut *self.de).map(Some),
        }
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, Error> {
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
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2);
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

    /// An event source that ends exactly where the test wants it to.
    ///
    /// The real parser reports its own error at end of input, so a stream that
    /// simply *stops* mid-collection is unreachable through [`Tape`]. Injecting
    /// one is the only way to prove the truncation guards fire.
    struct VecSource<'de> {
        events: std::vec::IntoIter<Event<'de>>,
    }

    impl<'de> VecSource<'de> {
        fn new(events: Vec<Event<'de>>) -> Self {
            Self {
                events: events.into_iter(),
            }
        }
    }

    impl<'de> EventSource<'de> for VecSource<'de> {
        fn next_event(&mut self) -> Option<CoreResult<Event<'de>>> {
            self.events.next().map(Ok)
        }
    }

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
        let mut source = VecSource::new(events);
        let mut de = EventDeserializer::new(&mut source, YamlVersion::V1_2);
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
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2);
        de.skip_framing().unwrap();
        assert_eq!(de.peek().unwrap(), Some(Peek::Value));
    }

    #[test]
    fn skip_framing_on_an_empty_document_reaches_the_end() {
        let mut tape = Tape::new("");
        let mut de = EventDeserializer::new(&mut tape, YamlVersion::V1_2);
        de.skip_framing().unwrap();
        assert_eq!(de.peek().unwrap(), None);
    }

    #[test]
    fn skip_framing_steps_over_every_framing_event() {
        // Injected rather than parsed so all four framing kinds appear in one
        // stream: a real document does not necessarily emit each of them, and a
        // framing kind that `skip_framing` failed to recognise would otherwise
        // surface later as a baffling "expected a value, found document-end".
        let mut source = VecSource::new(vec![
            at(EventKind::StreamStart),
            at(EventKind::DocumentStart {
                explicit: true,
                version: YamlVersion::V1_2,
            }),
            scalar("7"),
            at(EventKind::DocumentEnd { explicit: true }),
            at(EventKind::StreamEnd),
        ]);
        let mut de = EventDeserializer::new(&mut source, YamlVersion::V1_2);

        de.skip_framing().unwrap();
        assert_eq!(i64::deserialize(&mut de).unwrap(), 7);

        // The trailing framing is skipped just as the leading framing was.
        de.skip_framing().unwrap();
        assert_eq!(de.peek().unwrap(), None);
    }
}
