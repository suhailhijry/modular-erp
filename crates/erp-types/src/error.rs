//! Errors produced by value-type construction.
//!
//! These carry the type name so a failure deep in a deserialization tree says
//! *which* newtype rejected the input, not just that something was invalid.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{type_name}` must be a UUID")]
pub struct IdParseError {
    pub type_name: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{type_name}` cannot be negative (got {value})")]
pub struct NegativeCounter {
    pub type_name: &'static str,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidStringReason {
    #[error("must not be empty")]
    Empty,
    #[error("is {len} bytes, exceeding the maximum of {max}")]
    TooLong { len: usize, max: usize },
    #[error("contains {ch:?} at byte {index}, which is not permitted here")]
    ForbiddenChar { ch: char, index: usize },
    #[error("{0}")]
    Other(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("`{type_name}` {reason}")]
pub struct InvalidString {
    pub type_name: &'static str,
    pub reason: InvalidStringReason,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MoneyError {
    /// The reason `Money` has no `Add` impl: this failure has to be handled,
    /// and an operator would let it be ignored.
    #[error("cannot combine {left} and {right}: different currencies")]
    CurrencyMismatch {
        left: crate::CurrencyCode,
        right: crate::CurrencyCode,
    },
    #[error("arithmetic overflowed the representable range for {currency}")]
    Overflow { currency: crate::CurrencyCode },
    #[error("cannot divide by zero")]
    DivideByZero,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidCurrency {
    #[error("currency code must be exactly 3 characters, got {0}")]
    WrongLength(usize),
    #[error("currency code must be ASCII letters, got {0:?}")]
    NotAlphabetic(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("expected at least one element")]
pub struct Empty;
