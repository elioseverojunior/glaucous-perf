// SPDX-FileCopyrightText: Glaucus contributors
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! A `serde::Deserializer` over a single parser scalar event.
//!
//! Fills a typed target without a `Node` tree existing.

// Constructed only by tests until #53 wires collections and #57 routes
// `from_str`. Scoped to this module and tied to those issues -- delete it when
// #53 lands and the compiler will confirm it is no longer needed.
#![allow(dead_code)]

use std::borrow::Cow;

use glaucus_core::types::{ScalarStyle, Tag, YamlVersion};
use serde::de::{self, Visitor};

use crate::de::{CoreTag, Resolved, classify_plain, visit_core_tagged_value};
use crate::error::Error;

fn err(msg: impl std::fmt::Display) -> Error {
    <Error as de::Error>::custom(msg)
}

/// One scalar event, ready to be deserialised.
///
/// Holds the event's `Cow` rather than a `&str` so a **borrowed** scalar can be
/// handed to serde as borrowed. That is the point of the whole epic: the text
/// already points into the input, and copying it here would give up the saving
/// streaming exists to capture.
pub(crate) struct ScalarDeserializer<'de> {
    value: Cow<'de, str>,
    style: ScalarStyle,
    tag: Option<Tag<'de>>,
    version: YamlVersion,
}

impl<'de> ScalarDeserializer<'de> {
    pub(crate) const fn new(
        value: Cow<'de, str>,
        style: ScalarStyle,
        tag: Option<Tag<'de>>,
        version: YamlVersion,
    ) -> Self {
        Self {
            value,
            style,
            tag,
            version,
        }
    }

    /// The core-schema tag this scalar carries, if any.
    fn core_tag(&self) -> Option<CoreTag> {
        self.tag.as_ref().and_then(|t| CoreTag::from_uri(&t.value))
    }

    /// Hands the text to serde, **borrowed where it is borrowed**.
    ///
    /// `visit_borrowed_str` is what lets a `#[serde(borrow)] &'de str` field
    /// deserialise without a copy. `visit_str` would compile and pass every test
    /// that does not borrow, while silently forfeiting the reason to stream — so
    /// the distinction is load-bearing rather than stylistic.
    fn visit_text<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match self.value {
            Cow::Borrowed(s) => visitor.visit_borrowed_str(s),
            Cow::Owned(s) => visitor.visit_string(s),
        }
    }
}

impl<'de> de::Deserializer<'de> for ScalarDeserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        // An explicit core-schema tag overrides both implicit resolution and the
        // style shortcut: `!!int "456"` is the author overriding what the quotes
        // would otherwise imply.
        if let Some(tag) = self.core_tag() {
            return visit_core_tagged_value(tag, &self.value, self.version, visitor);
        }

        // Quoting is the author stating the value is text.
        if self.style != ScalarStyle::Plain {
            return self.visit_text(visitor);
        }

        // The ordering is shared with the tree path so the two cannot drift.
        match classify_plain(&self.value, self.version) {
            Resolved::Null => visitor.visit_unit(),
            Resolved::Bool(b) => visitor.visit_bool(b),
            Resolved::I64(i) => visitor.visit_i64(i),
            Resolved::U64(u) => visitor.visit_u64(u),
            Resolved::F64(f) => visitor.visit_f64(f),
            Resolved::Str => self.visit_text(visitor),
        }
    }

    /// The raw text, never classified.
    ///
    /// A `&str` target asked for text, so `1` is the one-character string rather
    /// than the integer — forwarding to `deserialize_any` would resolve it and
    /// fail the target.
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.visit_text(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        self.visit_text(visitor)
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        match classify_plain(&self.value, self.version) {
            Resolved::Bool(b) => visitor.visit_bool(b),
            _ => Err(err(format!("expected boolean, found `{}`", self.value))),
        }
    }

    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        glaucus_core::schema::resolve_int(&self.value).map_or_else(
            || Err(err(format!("expected integer, found `{}`", self.value))),
            |i| visitor.visit_i64(i),
        )
    }

    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        glaucus_core::schema::resolve_uint(&self.value).map_or_else(
            || Err(err(format!("expected unsigned, found `{}`", self.value))),
            |u| visitor.visit_u64(u),
        )
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        glaucus_core::schema::resolve_float(&self.value).map_or_else(
            || Err(err(format!("expected float, found `{}`", self.value))),
            |f| visitor.visit_f64(f),
        )
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.style == ScalarStyle::Plain && glaucus_core::schema::is_null(&self.value) {
            return visitor.visit_none();
        }
        visitor.visit_some(self)
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Error> {
        if self.style == ScalarStyle::Plain && glaucus_core::schema::is_null(&self.value) {
            return visitor.visit_unit();
        }
        Err(err("expected null"))
    }

    serde::forward_to_deserialize_any! {
        i8 i16 i32 i128 u8 u16 u32 u128 f32 char bytes byte_buf
        unit_struct newtype_struct seq tuple tuple_struct map struct
        enum identifier ignored_any
    }
}

#[cfg(test)]
mod tests {
    use super::ScalarDeserializer;
    use glaucus_core::types::{ScalarStyle, Tag, YamlVersion};
    use serde::Deserialize;
    use serde::de::Deserializer as _;
    use std::borrow::Cow;

    /// A plain scalar borrowed from the input, resolved under YAML 1.2.
    fn plain(value: &str) -> ScalarDeserializer<'_> {
        ScalarDeserializer::new(
            Cow::Borrowed(value),
            ScalarStyle::Plain,
            None,
            YamlVersion::V1_2,
        )
    }

    fn quoted(value: &str) -> ScalarDeserializer<'_> {
        ScalarDeserializer::new(
            Cow::Borrowed(value),
            ScalarStyle::DoubleQuoted,
            None,
            YamlVersion::V1_2,
        )
    }

    fn plain_1_1(value: &str) -> ScalarDeserializer<'_> {
        ScalarDeserializer::new(
            Cow::Borrowed(value),
            ScalarStyle::Plain,
            None,
            YamlVersion::V1_1,
        )
    }

    fn tagged<'a>(value: &'a str, uri: &'a str) -> ScalarDeserializer<'a> {
        ScalarDeserializer::new(
            Cow::Borrowed(value),
            ScalarStyle::Plain,
            Some(Tag {
                value: Cow::Borrowed(uri),
                span: glaucus_core::types::Span::point(glaucus_core::types::Position::start()),
            }),
            YamlVersion::V1_2,
        )
    }

    /// Routes through `deserialize_any`, which is where resolution happens.
    #[derive(Deserialize, PartialEq, Debug)]
    #[serde(untagged)]
    enum Any {
        Bool(bool),
        Int(i64),
        Float(f64),
        Text(String),
        Unit,
    }

    fn any(de: ScalarDeserializer<'_>) -> Any {
        Any::deserialize(de).expect("should deserialise")
    }

    // ─── the borrow, which is the point ─────────────────────────────

    /// A `&'de str` target only succeeds if `visit_borrowed_str` was used.
    /// `visit_str` would compile and pass every other test here while silently
    /// forfeiting the saving streaming exists to capture.
    #[test]
    fn a_borrowed_scalar_reaches_serde_borrowed() {
        let input = String::from("hello world");
        let borrowed: &str = <&str>::deserialize(plain(&input)).expect("must borrow");
        assert_eq!(borrowed, "hello world");
        assert!(
            std::ptr::eq(borrowed.as_ptr(), input.as_ptr()),
            "the &str must point INTO the input, not at a copy"
        );
    }

    #[test]
    fn an_owned_scalar_still_deserialises_as_a_string() {
        // A folded scalar arrives owned; it cannot be borrowed, and must not fail.
        let de = ScalarDeserializer::new(
            Cow::Owned("folded text".to_owned()),
            ScalarStyle::Plain,
            None,
            YamlVersion::V1_2,
        );
        assert_eq!(String::deserialize(de).unwrap(), "folded text");
    }

    // ─── resolution, through the shared core resolvers ──────────────

    #[test]
    fn radix_prefixes_resolve() {
        assert_eq!(any(plain("0x1F")), Any::Int(31));
        assert_eq!(any(plain("0X1f")), Any::Int(31));
        assert_eq!(any(plain("0o17")), Any::Int(15));
        assert_eq!(any(plain("-0x10")), Any::Int(-16));
        assert_eq!(
            any(plain("017")),
            Any::Int(17),
            "a bare leading zero is decimal"
        );
    }

    #[test]
    fn booleans_are_case_sensitive_and_version_aware() {
        assert_eq!(any(plain("true")), Any::Bool(true));
        assert_eq!(any(plain("TRUE")), Any::Bool(true));
        assert_eq!(any(plain("tRuE")), Any::Text("tRuE".into()));

        // 1.1 spellings are strings under 1.2 and booleans under 1.1.
        assert_eq!(any(plain("yes")), Any::Text("yes".into()));
        assert_eq!(any(plain_1_1("yes")), Any::Bool(true));
        assert_eq!(any(plain_1_1("no")), Any::Bool(false));
    }

    #[test]
    fn bare_inf_and_nan_stay_strings_dotted_forms_are_floats() {
        for word in ["inf", "Infinity", "nan", "NaN"] {
            assert_eq!(any(plain(word)), Any::Text(word.into()), "{word}");
        }
        assert!(matches!(any(plain(".inf")), Any::Float(f) if f.is_infinite()));
        assert!(
            matches!(any(plain("-.inf")), Any::Float(f) if f.is_infinite() && f.is_sign_negative())
        );
        assert!(matches!(any(plain(".nan")), Any::Float(f) if f.is_nan()));
    }

    #[test]
    fn null_resolves_and_options_see_none() {
        assert_eq!(any(plain("null")), Any::Unit);
        assert_eq!(any(plain("~")), Any::Unit);
        assert_eq!(Option::<String>::deserialize(plain("~")).unwrap(), None);
        assert_eq!(
            Option::<String>::deserialize(plain("text")).unwrap(),
            Some("text".to_owned())
        );
    }

    /// Quoting is the author stating the value is text.
    #[test]
    fn quoted_scalars_stay_strings_regardless_of_content() {
        for content in ["1", "true", "null", "0x1F", ".inf", ""] {
            assert_eq!(
                any(quoted(content)),
                Any::Text(content.into()),
                "{content:?} is quoted and must stay text"
            );
        }
        // A quoted null is not None.
        assert_eq!(
            Option::<String>::deserialize(quoted("~")).unwrap(),
            Some("~".to_owned())
        );
    }

    /// An explicit tag beats both implicit resolution and the style shortcut.
    #[test]
    fn core_schema_tags_drive_resolution() {
        assert_eq!(
            any(tagged("123", "tag:yaml.org,2002:str")),
            Any::Text("123".into())
        );
        assert_eq!(any(tagged("456", "tag:yaml.org,2002:int")), Any::Int(456));
        assert_eq!(
            any(tagged("true", "tag:yaml.org,2002:bool")),
            Any::Bool(true)
        );
        assert_eq!(any(tagged("null", "tag:yaml.org,2002:null")), Any::Unit);
        assert!(Any::deserialize(tagged("abc", "tag:yaml.org,2002:int")).is_err());
    }

    /// A `&str` target asked for TEXT, so `1` is the one-character string.
    /// Forwarding this to `deserialize_any` would resolve it and fail the target.
    #[test]
    fn a_str_target_is_never_classified() {
        assert_eq!(String::deserialize(plain("1")).unwrap(), "1");
        assert_eq!(String::deserialize(plain("true")).unwrap(), "true");
        assert_eq!(String::deserialize(plain("0x1F")).unwrap(), "0x1F");
    }

    #[test]
    fn typed_targets_use_the_shared_resolvers() {
        assert_eq!(i64::deserialize(plain("0x1F")).unwrap(), 31);
        assert_eq!(u64::deserialize(plain("0o17")).unwrap(), 15);
        assert!(
            u64::deserialize(plain("-1")).is_err(),
            "unsigned refuses a minus"
        );
        assert!((f64::deserialize(plain("1.5")).unwrap() - 1.5).abs() < f64::EPSILON);
        assert!(
            bool::deserialize(plain("yes")).is_err(),
            "not a 1.2 boolean"
        );
        assert!(bool::deserialize(plain_1_1("yes")).unwrap());
    }

    #[test]
    fn deserialize_unit_requires_a_plain_null() {
        assert!(
            plain("null")
                .deserialize_unit(serde::de::IgnoredAny)
                .is_ok()
        );
        assert!(
            plain("text")
                .deserialize_unit(serde::de::IgnoredAny)
                .is_err()
        );
        assert!(
            quoted("null")
                .deserialize_unit(serde::de::IgnoredAny)
                .is_err()
        );
    }
}
