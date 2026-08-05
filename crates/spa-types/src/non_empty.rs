//! A list that cannot be empty.
//!
//! Head-and-tail rather than a `Vec` with a private invariant, so `first()`
//! returns `&T` instead of `Option<&T>` — the guarantee shows up in the
//! signature rather than in a comment.
//!
//! This is what retires "entry has no lines" as a runtime error: a journal entry
//! holding `NonEmpty<JournalLine>` cannot be empty, so there is no case to check
//! and no error variant to define.

use serde::{Deserialize, Serialize};

use crate::error::Empty;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "Vec<T>",
    into = "Vec<T>",
    bound = "T: Clone + Serialize + serde::de::DeserializeOwned"
)]
pub struct NonEmpty<T> {
    head: T,
    tail: Vec<T>,
}

impl<T> NonEmpty<T> {
    #[must_use]
    pub const fn singleton(head: T) -> Self {
        Self {
            head,
            tail: Vec::new(),
        }
    }

    #[must_use]
    pub const fn new(head: T, tail: Vec<T>) -> Self {
        Self { head, tail }
    }

    pub fn try_from_vec(items: Vec<T>) -> Result<Self, Empty> {
        let mut iter = items.into_iter();
        let head = iter.next().ok_or(Empty)?;
        Ok(Self {
            head,
            tail: iter.collect(),
        })
    }

    /// Infallible, unlike `[T]::first`.
    #[must_use]
    pub const fn first(&self) -> &T {
        &self.head
    }

    #[must_use]
    pub fn last(&self) -> &T {
        self.tail.last().unwrap_or(&self.head)
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.tail.len() + 1
    }

    /// Always `false`. Present because clippy asks for it next to `len`, and
    /// because a caller writing generic code may reasonably call it.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    pub fn push(&mut self, item: T) {
        self.tail.push(item);
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        core::iter::once(&self.head).chain(self.tail.iter())
    }

    pub fn map<U, F: FnMut(&T) -> U>(&self, mut f: F) -> NonEmpty<U> {
        NonEmpty {
            head: f(&self.head),
            tail: self.tail.iter().map(&mut f).collect(),
        }
    }

    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        let mut out = Vec::with_capacity(self.len());
        out.push(self.head);
        out.extend(self.tail);
        out
    }
}

impl<T> IntoIterator for NonEmpty<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;
    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl<'a, T> IntoIterator for &'a NonEmpty<T> {
    type Item = &'a T;
    type IntoIter = Box<dyn Iterator<Item = &'a T> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<T> TryFrom<Vec<T>> for NonEmpty<T> {
    type Error = Empty;
    fn try_from(value: Vec<T>) -> Result<Self, Self::Error> {
        Self::try_from_vec(value)
    }
}

impl<T> From<NonEmpty<T>> for Vec<T> {
    fn from(value: NonEmpty<T>) -> Self {
        value.into_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_vec_is_rejected() {
        assert!(NonEmpty::<u8>::try_from_vec(vec![]).is_err());
    }

    #[test]
    fn first_and_last_need_no_option() {
        let list = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        assert_eq!(*list.first(), 1);
        assert_eq!(*list.last(), 3);
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn a_singleton_reports_the_same_head_and_last() {
        let list = NonEmpty::singleton(7);
        assert_eq!(*list.first(), 7);
        assert_eq!(*list.last(), 7);
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn iteration_visits_head_then_tail_in_order() {
        let list = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        assert_eq!(list.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(list.clone().into_iter().collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(list.map(|x| x * 2).into_vec(), vec![2, 4, 6]);
    }

    #[test]
    fn serde_round_trips_as_a_plain_array() {
        let list = NonEmpty::try_from_vec(vec![1, 2, 3]).unwrap();
        let json = serde_json::to_string(&list).unwrap();
        assert_eq!(json, "[1,2,3]");
        assert_eq!(serde_json::from_str::<NonEmpty<i32>>(&json).unwrap(), list);
    }

    #[test]
    fn deserializing_an_empty_array_fails() {
        // The guarantee has to survive the event log, not just Rust construction.
        assert!(serde_json::from_str::<NonEmpty<i32>>("[]").is_err());
    }
}
