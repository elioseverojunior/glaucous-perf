// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Differential harness: the tree path and the streaming path must agree.
//!
//! Written **before** the streaming engine exists, deliberately.
//!
//! Two implementations of anchor resolution that must agree is the same shape
//! that produced three divergent scalar resolvers. In #40, `glaucus schema
//! validate` and `glaucus from_str` reached opposite verdicts on `0x1F` because
//! the logic had been written three times and drifted — and nobody noticed until
//! someone compared them by hand.
//!
//! A harness added at the end of that work would have arrived to a backlog of
//! accumulated differences, with no way to tell which were regressions and which
//! had always been there. This one covers the corpus while there is still only
//! one engine, so it runs green from the start and the first disagreement it ever
//! reports is a real one, introduced by the commit that broke it.
//!
//! Until #57 routes `from_str` by target type, [`via_streaming`] calls the same
//! function as [`via_tree`] and every case passes trivially. **That is the
//! point.**

use glaucus_serde::{Value, from_str};

/// One corpus entry. The name is carried so a failure says which case broke.
struct Case {
    name: &'static str,
    yaml: &'static str,
}

/// Deserialises through the `Node` tree: compose, then walk the tree.
fn via_tree(yaml: &str) -> Result<Value, String> {
    from_str::<Value>(yaml).map_err(|e| e.to_string())
}

/// Deserialises through the parser event stream, without composing a tree.
///
/// Identical to [`via_tree`] until #57. Kept as a separate function so the switch
/// is a one-line change here rather than a rewrite of the harness, and so the
/// corpus is already green when the second engine arrives.
fn via_streaming(yaml: &str) -> Result<Value, String> {
    from_str::<Value>(yaml).map_err(|e| e.to_string())
}

/// Asserts both paths agree on whether `yaml` parses, and on what it produces.
///
/// Error *messages* are deliberately not compared. Two engines can reject the
/// same document for the same reason while wording it differently, and treating
/// that as a difference would bury the real signal. What must match is the
/// verdict — accept or reject — and, when accepted, the value.
///
/// The target is [`Value`] because it is shape-preserving. A typed struct would
/// silently discard any field the two paths disagreed about, which is exactly the
/// disagreement this exists to catch.
fn assert_paths_agree(name: &str, yaml: &str) -> Option<String> {
    match (via_tree(yaml), via_streaming(yaml)) {
        (Ok(tree), Ok(streaming)) => (tree != streaming).then(|| {
            format!("{name}: both paths accepted but produced different values\n    tree:      {tree:?}\n    streaming: {streaming:?}")
        }),
        (Err(_), Err(_)) => None,
        (Ok(tree), Err(e)) => Some(format!(
            "{name}: tree accepted, streaming REJECTED\n    tree:      {tree:?}\n    streaming: {e}"
        )),
        (Err(e), Ok(streaming)) => Some(format!(
            "{name}: tree REJECTED, streaming accepted\n    tree:      {e}\n    streaming: {streaming:?}"
        )),
    }
}

/// Twenty cases, chosen for where two engines are most likely to drift apart.
const CORPUS: &[Case] = &[
    // ─── scalars and resolution ─────────────────────────────────────
    Case {
        name: "plain-scalars",
        yaml: "a: 1\nb: text\nc: true\nd: null\ne: 1.5\n",
    },
    Case {
        name: "radix-integers",
        yaml: "hex: 0x1F\nHEX: 0X1f\noct: 0o17\nneg: -0x10\nzero: 017\n",
    },
    Case {
        name: "bare-inf-nan-are-strings",
        yaml: "a: inf\nb: nan\nc: infinity\nd: -inf\n",
    },
    Case {
        name: "dotted-inf-nan-are-floats",
        yaml: "a: .inf\nb: -.inf\nc: .nan\nd: .INF\n",
    },
    Case {
        name: "quoted-values-stay-text",
        yaml: "a: \"1\"\nb: 'true'\nc: \"null\"\nd: \"\"\n",
    },
    // ─── structure ──────────────────────────────────────────────────
    Case {
        name: "nested-collections",
        yaml: "a:\n  b:\n    - 1\n    - c: 2\n      d: [3, 4]\n",
    },
    Case {
        name: "empty-document",
        yaml: "",
    },
    Case {
        name: "comment-only-document",
        yaml: "# nothing but a comment\n",
    },
    Case {
        name: "non-string-mapping-keys",
        yaml: "1: one\ntrue: yes-key\n0x1F: hex-key\n",
    },
    // ─── anchors and aliases ────────────────────────────────────────
    Case {
        name: "anchor-on-scalar",
        yaml: "a: &x 1\nb: *x\n",
    },
    Case {
        name: "anchor-on-mapping",
        yaml: "a: &x {p: 1, q: 2}\nb: *x\n",
    },
    Case {
        name: "anchor-on-sequence",
        yaml: "a: &x [1, 2, 3]\nb: *x\n",
    },
    Case {
        name: "nested-anchors",
        yaml: "a: &outer\n  inner: &in [1, 2]\n  other: *in\nb: *outer\n",
    },
    Case {
        name: "anchor-aliased-repeatedly",
        yaml: "a: &x [1, 2]\nb: *x\nc: *x\nd: *x\ne: *x\n",
    },
    // ─── merge keys ─────────────────────────────────────────────────
    Case {
        name: "merge-simple",
        yaml: "d: &d {a: 1, b: 2}\nm:\n  <<: *d\n",
    },
    Case {
        name: "merge-overriding",
        yaml: "d: &d {a: 1, b: 2}\nm:\n  <<: *d\n  a: 9\n",
    },
    Case {
        name: "merge-sequence-of-sources",
        yaml: "x: &x {a: 1}\ny: &y {b: 2}\nm:\n  <<: [*x, *y]\n",
    },
    // ─── tags and directives ────────────────────────────────────────
    Case {
        name: "explicit-tags",
        yaml: "a: !!str 123\nb: !!int \"456\"\nc: !!bool \"true\"\nd: !!null \"null\"\n",
    },
    Case {
        name: "yaml-1-1-directive",
        yaml: "%YAML 1.1\n---\na: yes\nb: no\nc: on\nd: off\n",
    },
    Case {
        name: "binary-tag",
        yaml: "a: !!binary SGVsbG8gV29ybGQh\n",
    },
];

/// Malformed input. Agreeing about FAILURE matters as much as agreeing about
/// success: a streaming path that accepts what the tree path rejects has widened
/// what glaucus considers valid YAML without anyone deciding to.
const MALFORMED: &[Case] = &[
    Case {
        name: "unclosed-flow-sequence",
        yaml: "a: [1, 2\n",
    },
    Case {
        name: "unclosed-flow-mapping",
        yaml: "a: {b: 1\n",
    },
    Case {
        name: "undefined-alias",
        yaml: "a: *nope\n",
    },
    Case {
        name: "duplicate-key",
        yaml: "a: 1\na: 2\n",
    },
    Case {
        name: "tab-indentation",
        yaml: "a:\n\tb: 1\n",
    },
    Case {
        name: "bad-anchor-name",
        yaml: "a: &\nb: 1\n",
    },
];

#[test]
fn tree_and_streaming_paths_agree_on_the_corpus() {
    let disagreements: Vec<String> = CORPUS
        .iter()
        .filter_map(|c| assert_paths_agree(c.name, c.yaml))
        .collect();

    assert!(
        disagreements.is_empty(),
        "{} of {} corpus cases disagree:\n\n{}",
        disagreements.len(),
        CORPUS.len(),
        disagreements.join("\n\n")
    );
}

#[test]
fn tree_and_streaming_paths_agree_on_malformed_input() {
    let disagreements: Vec<String> = MALFORMED
        .iter()
        .filter_map(|c| assert_paths_agree(c.name, c.yaml))
        .collect();

    assert!(
        disagreements.is_empty(),
        "{} of {} malformed cases disagree:\n\n{}",
        disagreements.len(),
        MALFORMED.len(),
        disagreements.join("\n\n")
    );
}

/// Guards the harness itself.
///
/// Every case is collected and reported together rather than failing on the
/// first, so one broken case does not hide the other nineteen. This checks the
/// corpus is actually populated — a harness that silently iterates an empty list
/// passes forever and protects nothing, which is the failure mode #23 found in
/// the conformance suite.
#[test]
fn the_corpus_is_not_empty() {
    assert_eq!(CORPUS.len(), 20, "the corpus should hold twenty cases");
    assert!(!MALFORMED.is_empty(), "malformed cases must be covered too");

    let mut names: Vec<&str> = CORPUS.iter().chain(MALFORMED).map(|c| c.name).collect();
    names.sort_unstable();
    let before = names.len();
    names.dedup();
    assert_eq!(
        before,
        names.len(),
        "case names must be unique to identify failures"
    );
}
