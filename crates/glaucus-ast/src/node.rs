// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! The YAML representation tree (`Node`) and its constituent nodes.

use std::borrow::Cow;

use glaucus_core::types::{CollectionStyle, ScalarStyle, Span, Tag, YamlVersion};

// ─── Representation Graph ───────────────────────────────────────────

/// A YAML node in the representation graph.
///
/// The lifetime `'a` ties borrowed scalar values to the input source,
/// enabling zero-copy parsing for plain scalars.
#[derive(Debug, Clone, PartialEq)]
pub enum Node<'a> {
    /// A scalar (leaf) value.
    Scalar(Scalar<'a>),
    /// An ordered sequence of nodes.
    Sequence(Sequence<'a>),
    /// An ordered mapping of key-value pairs.
    Mapping(Mapping<'a>),
}

impl<'a> Node<'a> {
    /// Returns the span of this node in the source input.
    #[must_use]
    pub const fn span(&self) -> Span {
        match self {
            Node::Scalar(s) => s.span,
            Node::Sequence(s) => s.span,
            Node::Mapping(m) => m.span,
        }
    }

    /// Returns the tag of this node, if any.
    #[must_use]
    pub const fn tag(&self) -> Option<&Tag<'a>> {
        match self {
            Node::Scalar(s) => s.tag.as_ref(),
            Node::Sequence(s) => s.tag.as_ref(),
            Node::Mapping(m) => m.tag.as_ref(),
        }
    }

    /// Returns `true` if this node is a scalar.
    #[must_use]
    pub const fn is_scalar(&self) -> bool {
        matches!(self, Node::Scalar(_))
    }

    /// Returns `true` if this node is a sequence.
    #[must_use]
    pub const fn is_sequence(&self) -> bool {
        matches!(self, Node::Sequence(_))
    }

    /// Returns `true` if this node is a mapping.
    #[must_use]
    pub const fn is_mapping(&self) -> bool {
        matches!(self, Node::Mapping(_))
    }

    /// Returns the scalar value as a string slice, if this is a scalar node.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) => Some(&s.value),
            _ => None,
        }
    }

    /// The scalar's text if this is a **plain** (unquoted) scalar.
    ///
    /// Quoting is the author stating that the value is text, so every typed
    /// accessor below declines a quoted scalar. This helper is what enforces
    /// that: `"true"` is a string, not a boolean, and `""` is the empty string,
    /// not null.
    fn plain_text(&self) -> Option<&str> {
        match self {
            Node::Scalar(s) if s.style == ScalarStyle::Plain => Some(&s.value),
            _ => None,
        }
    }

    /// Returns `true` if this is a plain scalar resolving to null.
    ///
    /// That is `null`, `Null`, `NULL`, `~`, or the empty plain scalar. A quoted
    /// `"null"` is the four-character string and returns `false`, as does any
    /// collection.
    #[must_use]
    pub fn is_null(&self) -> bool {
        self.plain_text().is_some_and(glaucus_core::schema::is_null)
    }

    /// Resolves this node as a boolean, or `None`.
    ///
    /// **YAML 1.2 resolution only.** A `Node` carries no document version, so the
    /// 1.1 spellings — `yes`, `on`, `y` and their negatives — are strings here.
    /// Those stay with the deserialiser, which learns the version from a `%YAML`
    /// directive; inventing an answer without that information would be the
    /// Norway problem by another route.
    ///
    /// Matching is case-sensitive: `true`, `True` and `TRUE` resolve, `tRuE` does
    /// not.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        self.plain_text()
            .and_then(|t| glaucus_core::schema::resolve_bool(t, YamlVersion::V1_2))
    }

    /// Resolves this node as a signed integer, or `None`.
    ///
    /// Accepts the Core Schema radix prefixes: `0x`/`0X` and `0o`/`0O`. A bare
    /// leading zero is decimal — `017` is seventeen.
    #[must_use]
    pub fn as_i64(&self) -> Option<i64> {
        self.plain_text()
            .and_then(glaucus_core::schema::resolve_int)
    }

    /// Resolves this node as an unsigned integer, or `None`.
    ///
    /// A leading `-` is refused outright, so `-0` does not become zero.
    #[must_use]
    pub fn as_u64(&self) -> Option<u64> {
        self.plain_text()
            .and_then(glaucus_core::schema::resolve_uint)
    }

    /// Resolves this node as a float, or `None`.
    ///
    /// Only the dotted infinity and NaN forms resolve. Bare `inf`, `infinity` and
    /// `nan` are strings under YAML 1.2, whatever Rust's own parser accepts.
    #[must_use]
    pub fn as_f64(&self) -> Option<f64> {
        self.plain_text()
            .and_then(glaucus_core::schema::resolve_float)
    }

    /// Returns a reference to the sequence items, if this is a sequence node.
    #[must_use]
    pub fn as_sequence(&self) -> Option<&[Self]> {
        match self {
            Node::Sequence(s) => Some(&s.items),
            _ => None,
        }
    }

    /// Returns a reference to the mapping entries, if this is a mapping node.
    #[must_use]
    pub fn as_mapping(&self) -> Option<&[(Self, Self)]> {
        match self {
            Node::Mapping(m) => Some(&m.entries),
            _ => None,
        }
    }

    /// Converts this node into a `'static` lifetime by taking ownership of all borrowed data.
    #[must_use]
    pub fn into_owned(self) -> Node<'static> {
        match self {
            Node::Scalar(s) => Node::Scalar(s.into_owned()),
            Node::Sequence(s) => Node::Sequence(s.into_owned()),
            Node::Mapping(m) => Node::Mapping(m.into_owned()),
        }
    }
}

/// A YAML scalar (leaf) value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scalar<'a> {
    /// The scalar value, borrowing from the input when no transformation is needed.
    pub value: Cow<'a, str>,
    /// Optional YAML tag.
    pub tag: Option<Tag<'a>>,
    /// The presentation style used in the source.
    pub style: ScalarStyle,
    /// Source span.
    pub span: Span,
}

impl Scalar<'_> {
    /// Converts this scalar into a `'static` lifetime by taking ownership of borrowed data.
    #[must_use]
    pub fn into_owned(self) -> Scalar<'static> {
        Scalar {
            value: Cow::Owned(self.value.into_owned()),
            tag: self.tag.map(Tag::into_owned),
            style: self.style,
            span: self.span,
        }
    }
}

/// A YAML sequence (ordered list).
#[derive(Debug, Clone, PartialEq)]
pub struct Sequence<'a> {
    /// The items in the sequence.
    pub items: Vec<Node<'a>>,
    /// Optional YAML tag.
    pub tag: Option<Tag<'a>>,
    /// The presentation style used in the source.
    pub style: CollectionStyle,
    /// Source span.
    pub span: Span,
}

impl Sequence<'_> {
    /// Converts this sequence into a `'static` lifetime by taking ownership of borrowed data.
    #[must_use]
    pub fn into_owned(self) -> Sequence<'static> {
        Sequence {
            items: self.items.into_iter().map(Node::into_owned).collect(),
            tag: self.tag.map(Tag::into_owned),
            style: self.style,
            span: self.span,
        }
    }
}

/// A YAML mapping (ordered key-value pairs).
///
/// Uses `Vec<(Node, Node)>` instead of `HashMap` to preserve insertion order
/// (required by the YAML spec) and to support non-string keys.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapping<'a> {
    /// The key-value entries in insertion order.
    pub entries: Vec<(Node<'a>, Node<'a>)>,
    /// Optional YAML tag.
    pub tag: Option<Tag<'a>>,
    /// The presentation style used in the source.
    pub style: CollectionStyle,
    /// Source span.
    pub span: Span,
}

impl Mapping<'_> {
    /// Converts this mapping into a `'static` lifetime by taking ownership of borrowed data.
    #[must_use]
    pub fn into_owned(self) -> Mapping<'static> {
        Mapping {
            entries: self
                .entries
                .into_iter()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
            tag: self.tag.map(Tag::into_owned),
            style: self.style,
            span: self.span,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use glaucus_core::types::{Position, Span};

    use super::*;

    #[test]
    fn node_span_collections() {
        let span = Span {
            start: Position {
                offset: 1,
                line: 1,
                column: 2,
            },
            end: Position {
                offset: 4,
                line: 1,
                column: 5,
            },
        };
        let seq = Node::Sequence(Sequence {
            items: vec![],
            tag: None,
            style: CollectionStyle::Block,
            span,
        });
        assert_eq!(seq.span(), span);

        let map = Node::Mapping(Mapping {
            entries: vec![],
            tag: None,
            style: CollectionStyle::Block,
            span,
        });
        assert_eq!(map.span(), span);
    }

    #[test]
    fn as_str_on_non_scalar() {
        let seq = Node::Sequence(Sequence {
            items: vec![],
            tag: None,
            style: CollectionStyle::Block,
            span: Span::point(Position::start()),
        });
        assert!(seq.as_str().is_none());
    }

    #[test]
    fn node_accessors() {
        let scalar = Node::Scalar(Scalar {
            value: Cow::Borrowed("hello"),
            tag: None,
            style: ScalarStyle::Plain,
            span: Span::point(Position::start()),
        });
        assert!(scalar.is_scalar());
        assert!(!scalar.is_sequence());
        assert!(!scalar.is_mapping());
        assert_eq!(scalar.as_str(), Some("hello"));
        assert!(scalar.as_sequence().is_none());
        assert!(scalar.as_mapping().is_none());
    }

    #[test]
    fn node_sequence_accessor() {
        let seq = Node::Sequence(Sequence {
            items: vec![Node::Scalar(Scalar {
                value: Cow::Borrowed("item"),
                tag: None,
                style: ScalarStyle::Plain,
                span: Span::point(Position::start()),
            })],
            tag: None,
            style: CollectionStyle::Block,
            span: Span::point(Position::start()),
        });
        assert!(seq.is_sequence());
        assert_eq!(seq.as_sequence().unwrap().len(), 1);
    }

    #[test]
    fn scalar_into_owned() {
        let scalar = Scalar {
            value: Cow::Borrowed("hello"),
            tag: Some(Tag {
                value: Cow::Borrowed("!!str"),
                span: Span::point(Position::start()),
            }),
            style: ScalarStyle::Plain,
            span: Span::point(Position::start()),
        };
        let owned: Scalar<'static> = scalar.into_owned();
        assert_eq!(&*owned.value, "hello");
        assert_eq!(&*owned.tag.unwrap().value, "!!str");
    }

    #[test]
    fn node_into_owned_scalar() {
        let node = Node::Scalar(Scalar {
            value: Cow::Borrowed("test"),
            tag: None,
            style: ScalarStyle::Plain,
            span: Span::point(Position::start()),
        });
        let owned: Node<'static> = node.into_owned();
        assert_eq!(owned.as_str(), Some("test"));
    }

    #[test]
    fn node_into_owned_sequence() {
        let node = Node::Sequence(Sequence {
            items: vec![Node::Scalar(Scalar {
                value: Cow::Borrowed("item"),
                tag: None,
                style: ScalarStyle::Plain,
                span: Span::point(Position::start()),
            })],
            tag: None,
            style: CollectionStyle::Block,
            span: Span::point(Position::start()),
        });
        let owned: Node<'static> = node.into_owned();
        assert_eq!(owned.as_sequence().unwrap().len(), 1);
        assert_eq!(owned.as_sequence().unwrap()[0].as_str(), Some("item"));
    }

    #[test]
    fn node_into_owned_mapping() {
        let node = Node::Mapping(Mapping {
            entries: vec![(
                Node::Scalar(Scalar {
                    value: Cow::Borrowed("key"),
                    tag: None,
                    style: ScalarStyle::Plain,
                    span: Span::point(Position::start()),
                }),
                Node::Scalar(Scalar {
                    value: Cow::Borrowed("val"),
                    tag: None,
                    style: ScalarStyle::Plain,
                    span: Span::point(Position::start()),
                }),
            )],
            tag: None,
            style: CollectionStyle::Block,
            span: Span::point(Position::start()),
        });
        let owned: Node<'static> = node.into_owned();
        let entries = owned.as_mapping().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0.as_str(), Some("key"));
        assert_eq!(entries[0].1.as_str(), Some("val"));
    }

    #[test]
    fn node_mapping_accessor() {
        let map = Node::Mapping(Mapping {
            entries: vec![(
                Node::Scalar(Scalar {
                    value: Cow::Borrowed("key"),
                    tag: None,
                    style: ScalarStyle::Plain,
                    span: Span::point(Position::start()),
                }),
                Node::Scalar(Scalar {
                    value: Cow::Borrowed("value"),
                    tag: None,
                    style: ScalarStyle::Plain,
                    span: Span::point(Position::start()),
                }),
            )],
            tag: None,
            style: CollectionStyle::Block,
            span: Span::point(Position::start()),
        });
        assert!(map.is_mapping());
        assert_eq!(map.as_mapping().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod typed_accessor_tests {
    use crate::node::Node;

    fn node(src: &str) -> Node<'static> {
        crate::composer::Composer::new(src)
            .next()
            .unwrap()
            .unwrap()
            .into_owned()
    }

    /// The scalar under key `v`, so quoting survives composition.
    fn v(src: &str) -> Node<'static> {
        node(&format!("v: {src}\n"))
            .as_mapping()
            .unwrap()
            .first()
            .unwrap()
            .1
            .clone()
    }

    #[test]
    fn plain_scalars_resolve() {
        assert!(v("null").is_null());
        assert!(v("~").is_null());
        assert_eq!(v("true").as_bool(), Some(true));
        assert_eq!(v("TRUE").as_bool(), Some(true));
        assert_eq!(v("false").as_bool(), Some(false));
        assert_eq!(v("42").as_i64(), Some(42));
        assert_eq!(v("42").as_u64(), Some(42));
        assert_eq!(v("1.5").as_f64(), Some(1.5));
    }

    #[test]
    fn radix_prefixes_resolve_through_both_integer_accessors() {
        assert_eq!(v("0x1F").as_i64(), Some(31));
        assert_eq!(v("0x1F").as_u64(), Some(31));
        assert_eq!(v("0o17").as_i64(), Some(15));
        assert_eq!(
            v("017").as_i64(),
            Some(17),
            "a bare leading zero is decimal"
        );
    }

    #[test]
    fn negative_resolves_as_signed_but_not_unsigned() {
        assert_eq!(v("-1").as_i64(), Some(-1));
        assert_eq!(v("-1").as_u64(), None);
        assert_eq!(v("-0").as_u64(), None, "`-0` must not become zero");
    }

    /// Quoting is the author stating the value is text. Every typed accessor must
    /// check the style, not just the characters.
    #[test]
    fn quoted_scalars_decline_every_typed_accessor() {
        for src in ["\"true\"", "'true'"] {
            let n = v(src);
            assert_eq!(n.as_bool(), None, "{src} is text");
            assert_eq!(n.as_str(), Some("true"), "{src} still reads as a string");
        }
        assert_eq!(v("\"42\"").as_i64(), None);
        assert_eq!(v("\"42\"").as_u64(), None);
        assert_eq!(v("\"1.5\"").as_f64(), None);
        assert!(!v("\"null\"").is_null(), "a quoted null is a string");
        assert!(
            !v("\"\"").is_null(),
            "an empty quoted scalar is the empty string"
        );
    }

    #[test]
    fn an_empty_plain_scalar_is_null() {
        assert!(v("").is_null());
    }

    #[test]
    fn collections_decline_every_typed_accessor() {
        for src in ["[1]", "{a: 1}"] {
            let n = v(src);
            assert!(!n.is_null(), "{src}");
            assert_eq!(n.as_bool(), None, "{src}");
            assert_eq!(n.as_i64(), None, "{src}");
            assert_eq!(n.as_u64(), None, "{src}");
            assert_eq!(n.as_f64(), None, "{src}");
        }
    }

    /// A `Node` carries no document version, so 1.1 boolean spellings are strings
    /// here. Resolving them would be the Norway problem by another route: the
    /// deserialiser answers differently because it can see the `%YAML` directive.
    #[test]
    fn yaml_1_1_boolean_spellings_are_strings_on_a_node() {
        for src in ["yes", "no", "on", "off", "y", "n", "Yes", "NO"] {
            assert_eq!(v(src).as_bool(), None, "{src} is a string on a Node");
            assert_eq!(v(src).as_str(), Some(src));
        }
    }

    #[test]
    fn boolean_matching_is_case_sensitive() {
        assert_eq!(v("tRuE").as_bool(), None);
        assert_eq!(v("fAlSe").as_bool(), None);
    }

    /// #26 travels with the resolvers: bare words are strings, dotted forms are
    /// floats.
    #[test]
    fn bare_inf_and_nan_are_strings_dotted_forms_are_floats() {
        for src in ["inf", "Infinity", "nan"] {
            assert_eq!(v(src).as_f64(), None, "{src} is a string");
        }
        assert!(v(".inf").as_f64().is_some_and(f64::is_infinite));
        assert!(
            v("-.inf")
                .as_f64()
                .is_some_and(|f| f.is_infinite() && f.is_sign_negative())
        );
        assert!(v(".nan").as_f64().is_some_and(f64::is_nan));
    }
}
