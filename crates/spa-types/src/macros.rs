//! Newtype constructors.
//!
//! The prototype carried a live defect — `sequence` (per-aggregate) and `id`
//! (global log position) were both `u64`, so writing one where the other
//! belonged compiled and silently corrupted retry accounting. These macros make
//! that class of mistake a type error, cheaply enough that there is no excuse
//! for a bare `String` or `i64` crossing a module boundary.

/// A UUID-backed identifier.
///
/// Generates `new_v7` (time-ordered, for index locality), `Display`, `FromStr`,
/// serde, and — behind the `sqlx` feature — `Type`/`Encode`/`Decode` so the type
/// can be bound directly in queries.
#[macro_export]
macro_rules! uuid_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
                 ::serde::Serialize, ::serde::Deserialize)]
        #[serde(transparent)]
        #[cfg_attr(feature = "sqlx", derive(::sqlx::Type))]
        #[cfg_attr(feature = "sqlx", sqlx(transparent))]
        pub struct $name(::uuid::Uuid);

        impl $name {
            /// A fresh time-ordered identifier.
            ///
            /// Deliberately not a `Default` impl: `Default` on an identifier
            /// would let `#[derive(Default)]` on any containing struct silently
            /// mint real ids, which is a footgun in a system where identity is
            /// the routing key for tenant data.
            #[must_use]
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(::uuid::Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(inner: ::uuid::Uuid) -> Self {
                Self(inner)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> &::uuid::Uuid {
                &self.0
            }

            #[must_use]
            pub const fn into_uuid(self) -> ::uuid::Uuid {
                self.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = $crate::IdParseError;
            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                s.parse::<::uuid::Uuid>()
                    .map(Self)
                    .map_err(|_| $crate::IdParseError {
                        type_name: stringify!($name),
                    })
            }
        }
    };
}

/// A monotonically increasing position or counter.
///
/// Deliberately supports **no arithmetic beyond `next`** and no conversion
/// between position types. `LogPosition` and `Sequence` are both "a number that
/// goes up", and confusing them is precisely the defect this exists to prevent —
/// so there is no `From`, no `as`, and no way to add one to the other.
#[macro_export]
macro_rules! counter {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
                 Default, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(transparent)]
        #[cfg_attr(feature = "sqlx", derive(::sqlx::Type))]
        #[cfg_attr(feature = "sqlx", sqlx(transparent))]
        pub struct $name(i64);

        impl $name {
            /// The position before any record exists. Checkpoints start here.
            pub const ZERO: Self = Self(0);

            /// The first real one. Mostly useful as `SchemaVersion::ONE`, which
            /// every event declares before it has a second shape.
            pub const ONE: Self = Self(1);

            /// Fails on negatives: Postgres `BIGINT` columns are signed, but
            /// these quantities never are, and a negative reaching a query is a
            /// bug worth catching at the boundary.
            pub fn new(value: i64) -> ::core::result::Result<Self, $crate::NegativeCounter> {
                if value < 0 {
                    return ::core::result::Result::Err($crate::NegativeCounter {
                        type_name: stringify!($name),
                        value,
                    });
                }
                ::core::result::Result::Ok(Self(value))
            }

            #[must_use]
            pub const fn get(self) -> i64 {
                self.0
            }

            /// The next position. Saturates rather than wrapping — at `i64::MAX`
            /// the system has other problems, but silently wrapping to a
            /// position that already exists is not one it should also have.
            #[must_use]
            pub const fn next(self) -> Self {
                Self(self.0.saturating_add(1))
            }

            /// How many steps ahead `self` is of `earlier`, saturating at zero.
            /// Used for lag metrics, never for arithmetic on positions.
            #[must_use]
            pub const fn distance_from(self, earlier: Self) -> u64 {
                if self.0 <= earlier.0 {
                    0
                } else {
                    // Sign-safe: the branch above proves the difference is
                    // positive, and both operands are non-negative by the
                    // constructor's invariant.
                    self.0.saturating_sub(earlier.0).cast_unsigned()
                }
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

/// A validated string newtype.
///
/// `$validate` receives `&str` and returns `Result<(), InvalidString>`. The inner
/// field is private and there is no `From<String>`, so the only way to construct
/// one is through validation.
#[macro_export]
macro_rules! validated_string {
    (
        $(#[$meta:meta])*
        $name:ident,
        max_len = $max:expr,
        validate = $validate:expr
    ) => {
        // Deliberately no `sqlx::Type` derive. `#[sqlx(transparent)]` would
        // generate a `Decode` that constructs the type without running
        // `new`, so a value read back from the database would skip validation —
        // exactly the gap that matters, since that is where data written by
        // older versions of the system arrives. Callers bind with `as_str()`
        // and decode through `new`, which keeps the check visible.
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, ::serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub const MAX_LEN: usize = $max;

            pub fn new(
                value: impl Into<String>,
            ) -> ::core::result::Result<Self, $crate::InvalidString> {
                let value: String = value.into();
                if value.is_empty() {
                    return ::core::result::Result::Err($crate::InvalidString {
                        type_name: stringify!($name),
                        reason: $crate::InvalidStringReason::Empty,
                    });
                }
                if value.len() > Self::MAX_LEN {
                    return ::core::result::Result::Err($crate::InvalidString {
                        type_name: stringify!($name),
                        reason: $crate::InvalidStringReason::TooLong {
                            len: value.len(),
                            max: Self::MAX_LEN,
                        },
                    });
                }
                let validate: fn(&str) -> ::core::result::Result<(), $crate::InvalidStringReason> =
                    $validate;
                validate(&value).map_err(|reason| $crate::InvalidString {
                    type_name: stringify!($name),
                    reason,
                })?;
                ::core::result::Result::Ok(Self(value))
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl ::core::fmt::Display for $name {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                ::core::fmt::Display::fmt(&self.0, f)
            }
        }

        impl ::core::str::FromStr for $name {
            type Err = $crate::InvalidString;
            fn from_str(s: &str) -> ::core::result::Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        // Deserialization goes through `new`, so a value read from the event log
        // or an API body is validated on the way in. Without this, `serde` would
        // happily construct an invalid instance and the guarantee would hold
        // only for values built in Rust.
        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> ::core::result::Result<Self, D::Error>
            where
                D: ::serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(::serde::de::Error::custom)
            }
        }
    };
}
