//! Shared selector machinery: `Known<T>`, EnumVariants, and the selector_enum! macro.
//!
//! Split from the former monolithic `params.rs` (review §15). Every
//! public item is re-exported from `mcp::params`, so external paths
//! are unchanged.

use serde::{Deserialize, Serialize};

/// A selector value that is either one of the known variants or an
/// unrecognized string. `Known` gives typed dispatch + closed schemas;
/// `Other` keeps forward compatibility honest — new/typo'd values surface as
/// envelope `invalid_request` naming every accepted value, never as a
/// transport-level parse failure with no context.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum Known<T> {
    Known(T),
    Other(String),
}

impl<T> Known<T> {
    /// The known variant, or `None` when the caller passed an unrecognized
    /// value.
    pub fn known(&self) -> Option<&T> {
        match self {
            Known::Known(t) => Some(t),
            Known::Other(_) => None,
        }
    }
}

/// `From<&str>` for test/authoring ergonomics: a recognized wire name
/// becomes `Known`, anything else becomes `Known::Other` (which the
/// handlers surface as `invalid_request` naming the accepted set). The MCP
/// transport path never uses this — serde's untagged derive handles the
/// wire — but test constructors and scenario authors write `"text".into()`.
impl<T: std::str::FromStr<Err = String>> From<&str> for Known<T> {
    fn from(s: &str) -> Self {
        match T::from_str(s) {
            Ok(t) => Known::Known(t),
            Err(_) => Known::Other(s.to_string()),
        }
    }
}

/// Trait for the closed selector enums: their variant names, for error
/// messages and the capability registry. Hand-rolled (no strum dependency).
pub trait EnumVariants {
    const VARIANTS: &'static [&'static str];
}

macro_rules! selector_enum {
    (
        $(#[$meta:meta])*
        $name:ident ; [ $( $variant:ident => $wire:literal ),+ $(,)? ]
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
        pub enum $name {
            $(
                #[serde(rename = $wire)]
                $variant,
            )+
        }

        impl EnumVariants for $name {
            const VARIANTS: &'static [&'static str] = &[ $( $wire ),+ ];
        }

        impl $name {
            /// Wire name of this variant (the exact historical string the
            /// handler matched before the enum existed).
            pub fn as_str(&self) -> &'static str {
                match self {
                    $( $name::$variant => $wire, )+
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $wire => Ok($name::$variant), )+
                    other => Err(format!(
                        "unknown {} '{}' (expected one of: {})",
                        stringify!($name), other,
                        Self::VARIANTS.join(", ")
                    )),
                }
            }
        }
    };
}

// macro 2.0 path-based re-export: sibling family modules invoke this macro
// via `use super::common::selector_enum;` (textual scoping of macro_rules!
// does not cross module files by itself).
pub(crate) use selector_enum;
