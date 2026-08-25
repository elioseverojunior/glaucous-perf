// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A zero-copy YAML value borrowing scalar text directly from the input.
//!
//! [`BorrowedValue`] wraps a [`Node<'a>`] whose scalar slices point directly
//! into the source string — no heap allocation for plain, unescaped scalars.
//! This is the borrowing parallel to the owned [`Value`](crate::Value), which
//! wraps a `Node<'static>`.

use glaucus_ast::node::Node;
use glaucus_core::error::{Error, ErrorKind, Result};
use std::ops::Deref;

/// A YAML value that borrows scalar slices directly from the source string.
///
/// Plain (unescaped) scalars are zero-copy: the `Cow<'a, str>` inside each
/// `Scalar` node is `Cow::Borrowed`, pointing into `input` without any heap
/// allocation.  Only scalars that require transformation (escape sequences,
/// block-scalar folding, etc.) allocate an owned `String`.
///
/// Use [`BorrowedValue::parse`] to construct one from a `&str`.
///
/// # Comparison with [`Value`](crate::Value)
///
/// | | [`Value`](crate::Value) | `BorrowedValue<'a>` |
/// |---|---|---|
/// | Lifetime | `'static` (owns all data) | borrows from input |
/// | Plain scalars | heap-allocated copy | zero-copy borrow |
/// | Use case | long-lived, send across threads | short-lived parsing |
#[derive(Debug, Clone, PartialEq)]
/// # The `Node` surface
///
/// This type derefs to the [`Node`] it wraps, so the whole node surface —
/// `is_null`, `as_str`, `as_bool`, `as_i64`, `as_u64`, `as_f64`, `as_mapping`,
/// `as_sequence`, `get_path` and `query` — is available directly on it. They are
/// **not** re-declared here: inherent methods would shadow the deref with
/// identical bodies, so the duplication would buy nothing and could drift.
///
/// `get_path` and `query` hand back `&Node`, not a wrapped type. Returning a
/// borrowed wrapper would need a `repr(transparent)` pointer cast, which
/// `#![forbid(unsafe_code)]` rules out, and the `Node` carries the same
/// accessors — so nothing is lost.
pub struct BorrowedValue<'a>(Node<'a>);

impl<'a> BorrowedValue<'a> {
    /// Parses the first document of `input`, borrowing scalars from it.
    ///
    /// Returns [`ErrorKind::UnexpectedEof`] when `input` contains no document.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is not valid YAML or is empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use glaucus_serde::BorrowedValue;
    ///
    /// let input = String::from("hello");
    /// let bv = BorrowedValue::parse(&input).unwrap();
    /// assert_eq!(bv.as_str(), Some("hello"));
    /// ```
    pub fn parse(input: &'a str) -> Result<Self> {
        match glaucus_ast::composer::Composer::new(input).next() {
            Some(node) => Ok(BorrowedValue(node?)),
            None => Err(Error::spanless(ErrorKind::UnexpectedEof)),
        }
    }

    /// Wraps an existing borrowed node.
    #[must_use]
    pub const fn new(node: Node<'a>) -> Self {
        BorrowedValue(node)
    }

    /// Returns a reference to the inner node.
    #[must_use]
    pub const fn as_node(&self) -> &Node<'a> {
        &self.0
    }

    /// Clones every borrowed scalar so the value outlives its source.
    ///
    /// A `BorrowedValue` cannot escape the `&str` it was parsed from. This is the
    /// exit: it pays the copy once, at the point the caller chooses, rather than
    /// on every access.
    ///
    /// # Examples
    ///
    /// ```
    /// use glaucus_serde::{BorrowedValue, Value};
    ///
    /// let owned: Value = {
    ///     let src = String::from("port: 8080\n");
    ///     BorrowedValue::parse(&src).unwrap().into_owned()
    /// }; // `src` is dropped here
    ///
    /// assert_eq!(owned.get_path("port").and_then(|n| n.as_i64()), Some(8080));
    /// ```
    #[must_use]
    pub fn into_owned(self) -> crate::value::Value {
        crate::value::Value::new(self.0.into_owned())
    }

    /// Consumes self, returning the inner node.
    #[must_use]
    pub fn into_node(self) -> Node<'a> {
        self.0
    }
}

impl<'a> Deref for BorrowedValue<'a> {
    type Target = Node<'a>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> From<Node<'a>> for BorrowedValue<'a> {
    fn from(n: Node<'a>) -> Self {
        BorrowedValue(n)
    }
}

impl<'a> From<BorrowedValue<'a>> for Node<'a> {
    fn from(bv: BorrowedValue<'a>) -> Self {
        bv.0
    }
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn borrowed_value_is_zero_copy_for_plain_scalars() {
        let input = String::from("hello world");
        let bv = BorrowedValue::parse(&input).unwrap();
        assert_eq!(bv.as_str(), Some("hello world"));
        if let Node::Scalar(s) = bv.as_node() {
            assert!(
                matches!(s.value, Cow::Borrowed(_)),
                "plain scalar must borrow, not own"
            );
        } else {
            panic!("expected scalar");
        }
    }

    #[test]
    fn borrowed_value_traverses_mapping() {
        let input = String::from("a: 1\nb: two\n");
        let bv = BorrowedValue::parse(&input).unwrap();
        let m = bv.as_node().as_mapping().unwrap();
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn borrowed_value_empty_input_returns_unexpected_eof() {
        let result = BorrowedValue::parse("");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::UnexpectedEof),
            "expected UnexpectedEof, got {:?}",
            err.kind
        );
    }

    #[test]
    fn borrowed_value_new_wraps_node() {
        let input = String::from("42");
        let node = glaucus_ast::composer::Composer::new(&input)
            .next()
            .unwrap()
            .unwrap();
        let bv = BorrowedValue::new(node);
        assert_eq!(bv.as_str(), Some("42"));
    }

    #[test]
    fn borrowed_value_into_node_round_trips() {
        let input = String::from("round: trip");
        let bv = BorrowedValue::parse(&input).unwrap();
        let node = bv.into_node();
        assert!(node.as_mapping().is_some());
    }

    #[test]
    fn borrowed_value_from_node_conversion() {
        let input = String::from("from: conversion");
        let node: Node<'_> = glaucus_ast::composer::Composer::new(&input)
            .next()
            .unwrap()
            .unwrap();
        let bv: BorrowedValue<'_> = BorrowedValue::from(node);
        assert!(bv.is_mapping());
    }

    #[test]
    fn borrowed_value_into_node_via_from_impl() {
        let input = String::from("key: val");
        let bv = BorrowedValue::parse(&input).unwrap();
        let node: Node<'_> = Node::from(bv);
        assert!(node.as_mapping().is_some());
    }

    #[test]
    fn borrowed_value_deref_exposes_node_methods() {
        let input = String::from("- a\n- b\n");
        let bv = BorrowedValue::parse(&input).unwrap();
        // Deref lets us call Node methods directly.
        assert!(bv.is_sequence());
        assert_eq!(bv.as_sequence().unwrap().len(), 2);
    }

    #[test]
    fn borrowed_value_sequence_items_borrow_via_flow_sequence() {
        // Block-sequence items (`- val\n`) are Cow::Owned because the scanner
        // must look past the trailing newline to determine scalar boundaries,
        // setting all_borrowed = false.  Flow sequences on a single line have
        // no cross-line lookahead, so items remain zero-copy.
        let input = String::from("[alpha, beta]");
        let bv = BorrowedValue::parse(&input).unwrap();
        let items = bv.as_sequence().unwrap();
        assert_eq!(items.len(), 2);
        for item in items {
            if let Node::Scalar(s) = item {
                assert!(
                    matches!(s.value, Cow::Borrowed(_)),
                    "flow-sequence scalar items must borrow: got {:?}",
                    s.value
                );
            } else {
                panic!("expected scalar item");
            }
        }
    }
}

#[cfg(test)]
mod forwarded_surface_tests {
    use super::BorrowedValue;
    use crate::value::Value;

    const DOC: &str = "n: 0x1F\nneg: -1\nflag: true\nnothing: ~\ntext: hello\n\
                       f: 1.5\nitems:\n  - a\n  - b\nnested:\n  n: 7\n";

    /// Every accessor, on the borrowed type. Reached through `Deref`, which is
    /// why they are not re-declared as inherent methods.
    #[test]
    fn borrowed_value_exposes_the_whole_node_surface() {
        let src = DOC.to_owned();
        let v = BorrowedValue::parse(&src).unwrap();

        assert!(v.get_path("nothing").unwrap().is_null());
        assert_eq!(v.get_path("text").unwrap().as_str(), Some("hello"));
        assert_eq!(v.get_path("flag").unwrap().as_bool(), Some(true));
        assert_eq!(v.get_path("neg").unwrap().as_i64(), Some(-1));
        assert_eq!(v.get_path("neg").unwrap().as_u64(), None);
        assert_eq!(v.get_path("f").unwrap().as_f64(), Some(1.5));
        assert!(v.as_mapping().is_some());
        assert_eq!(
            v.get_path("items").unwrap().as_sequence().map(<[_]>::len),
            Some(2)
        );
        assert_eq!(v.query("..n").len(), 2);
    }

    /// Confirms the chain reaches the core resolvers: a bare `parse::<i64>()`
    /// would not know `0x`.
    #[test]
    fn radix_prefixed_integers_resolve_through_the_forwarded_accessor() {
        let src = DOC.to_owned();
        let v = BorrowedValue::parse(&src).unwrap();
        assert_eq!(v.get_path("n").unwrap().as_i64(), Some(31));
        assert_eq!(v.get_path("n").unwrap().as_u64(), Some(31));
    }

    /// The borrow must genuinely end, so the source is dropped inside the block.
    #[test]
    fn into_owned_survives_the_source_being_dropped() {
        let owned: Value = {
            let src = DOC.to_owned();
            BorrowedValue::parse(&src).unwrap().into_owned()
        };

        assert_eq!(owned.get_path("n").unwrap().as_i64(), Some(31));
        assert_eq!(owned.get_path("text").unwrap().as_str(), Some("hello"));
        assert_eq!(owned.query("..n").len(), 2);
    }

    #[test]
    fn owned_value_exposes_the_whole_node_surface() {
        let v: Value = crate::from_str(DOC).unwrap();

        assert!(v.get_path("nothing").unwrap().is_null());
        assert_eq!(v.get_path("text").unwrap().as_str(), Some("hello"));
        assert_eq!(v.get_path("flag").unwrap().as_bool(), Some(true));
        assert_eq!(v.get_path("n").unwrap().as_i64(), Some(31));
        assert_eq!(v.get_path("neg").unwrap().as_u64(), None);
        assert_eq!(v.get_path("f").unwrap().as_f64(), Some(1.5));
        assert!(v.as_mapping().is_some());
        assert_eq!(
            v.get_path("items").unwrap().as_sequence().map(<[_]>::len),
            Some(2)
        );
        assert_eq!(v.query("..n").len(), 2);
    }
}
