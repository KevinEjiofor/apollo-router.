#![deny(rustdoc::broken_intra_doc_links)]

use itertools::Itertools;

/// An extension trait for `Iterator`, similar to `Itertools`, that seeks to improve the ergonomics
/// around fallible operations.
///
/// "Fallible" here has a few different meanings. The operation that you are performing (such as a
/// `filter`) might yield a `Result` containing the value you actually want/need, but fallible can
/// also refer to the stream of items that you're iterating over (or both!). As much as possible, I
/// will use the following naming scheme in order to keep these ideas consistent:
///  - If the iterator yields an arbitrary `T` and the operation that you wish to apply is of the
///    form `T -> Result`, then it will named `fallible_*`.
///  - If the iterator yields `Result<T>` and the operation is of the form `T -> U` (for arbitrary
///    `U`), then it will named `*_ok`.
///  - If both iterator and operation yield `Result`, then it will named `and_then_*` (more on that
///    fewer down).
///
/// The first category mostly describes combinators that take closures that need specific types,
/// such as `filter` and things in the `any`/`all`/`find`/`fold` family. There are several
/// experiment features in `std` that offer similar functionalities.
///
/// The second category is mostly taken care of by `Itertools`. While they are not currently
/// implemented here (or in `Itertools`), this category would also contain methods like `*_err`
/// in addition to the "ok" methods.
///
/// The third category is the hardest to pin down. There are a ton of ways that you can combine two
/// results (just look at the docs page for `Result`), but, in general, the most common use case
/// that needs to be captured is the use of the `?` operator. For example, if you have a check that
/// is fallible, you likely will write that code like so:
/// ```no_test
/// for val in things {
///   let val = fallible_transformation(val)?;
///   if fallible_check(&val)? { continue }
///   process_value(val);
/// }
/// ```
/// In such a case, `process_value` is called if and only if both the transformation and check
/// return `Ok`. This is why methods in this category are named `and_then_*`.
///
/// There are, of course, methods that fall out of this taxonimy, but this covers the broad
/// strokes.
///
/// In all cases, errors are passed along until they are processed in some way. This includes
/// `Result::collect`, but also includes things like `Itertools::process_results` and things like
/// the `find` family.
///
/// Lastly, if you come across something that fits what this trait is trying to do and you have a
/// usecase for but that is not served by already, feel free to expand the functionalities!
// TODO: In std, methods like `all` and `any` are actually just specializations of `try_fold` using
// bools and `FlowControl`. When initially writing this, I, @TylerBloom, didn't take the time to
// write equalivalent folding methods. Should they be implemented in the future, we should rework
// existing methods to use them.
pub(crate) trait FallibleIterator: Sized + Itertools {
    /// The method transforms the existing iterator, which yields `T`s, into an iterator that
    /// yields `Result<T, E>`. The predicate that is provided is fallible. If the predicate yields
    /// `Ok(false)`, the item is skipped. If the predicate yields `Err`, that `T` is discard and
    /// the iterator will yield the `Err` in its place. Lastly, if the predicate yields `Ok(true)`,
    /// the iterator will yield `Ok(val)`.
    ///
    /// ```ignore
    /// // A totally accurate prime checker
    /// fn is_prime(i: usize) -> Result<bool, ()> {
    ///   match i {
    ///     0 | 1 => Err(()), // 0 and 1 are neither prime or composite
    ///     2 | 3 => Ok(true),
    ///     _ => Ok(false), // Every other number is composite, I guess
    ///   }
    /// }
    ///
    /// let vals = (1..6).fallible_filter(|i| is_prime(*i));
    /// itertools::assert_equal(vals, vec![Err(()), Ok(2), Ok(3)]);
    /// ```
    fn fallible_filter<F, E>(self, predicate: F) -> FallibleFilter<Self, F>
    where
        F: FnMut(&Self::Item) -> Result<bool, E>,
    {
        FallibleFilter {
            iter: self,
            predicate,
        }
    }

    // NOTE: There is a `filter_ok` method in `Itertools`, but there is not a `filter_err`. That
    // might be useful at some point.

    /// This method functions similarly to `Iterator::filter` but where the existing iterator
    /// yields `Result`s and the given predicate also returns `Result`s.
    ///
    /// The predicate is only called if the existing iterator yields `Ok`. `Err`s are ignored.
    /// Should the predicate return an `Err`, the `Ok` value was replaced with the `Err`. This
    /// method is very similar to `Itertools::filter_ok` except the predicate for this method is
    /// fallible.
    ///
    /// ```ignore
    /// // A totally accurate prime checker
    /// fn is_prime(i: usize) -> Result<bool, ()> {
    ///   match i {
    ///     0 | 1 => Err(()), // 0 and 1 are neither prime or composite
    ///     2 | 3 => Ok(true),
    ///     _ => Ok(false), // Every other number is composite, I guess
    ///   }
    /// }
    ///
    /// let vals = vec![Ok(0), Err(()), Err(()), Ok(3), Ok(4)].into_iter().and_then_filter(|i| is_prime(*i));
    /// itertools::assert_equal(vals, vec![Err(()), Err(()), Err(()), Ok(3)]);
    /// ```
    fn and_then_filter<T, E, F>(self, predicate: F) -> AndThenFilter<Self, F>
    where
        Self: Iterator<Item = Result<T, E>>,
        F: FnMut(&T) -> Result<bool, E>,
    {
        AndThenFilter {
            iter: self,
            predicate,
        }
    }

    /// This method functions similarly to `Iterator::all` but where the given predicate returns
    /// `Result`s.
    ///
    /// Like `Iterator::all`, this function short-circuits but will short-circuit if the predicate
    /// returns anything other than `Ok(true)`. If the first item that is not `Ok(true)` is
    /// `Ok(false)`, the returned value will be `Ok(false)`. If that item is `Err`, than that `Err`
    /// is returned.
    ///
    /// ```ignore
    /// // A totally accurate prime checker
    /// fn is_prime(i: usize) -> Result<bool, ()> {
    ///   match i {
    ///     0 | 1 => Err(()), // 0 and 1 are neither prime or composite
    ///     2 | 3 => Ok(true),
    ///     _ => Ok(false), // Every other number is composite, I guess
    ///   }
    /// }
    ///
    /// assert_eq!(Ok(true), [].into_iter().fallible_all(is_prime));
    /// assert_eq!(Ok(true), (2..4).fallible_all(is_prime));
    /// assert_eq!(Err(()), (1..4).fallible_all(is_prime));
    /// assert_eq!(Ok(false), (2..5).fallible_all(is_prime));
    /// assert_eq!(Err(()), (1..5).fallible_all(is_prime));
    /// ```
    fn fallible_all<E, F>(&mut self, mut predicate: F) -> Result<bool, E>
    where
        F: FnMut(Self::Item) -> Result<bool, E>,
    {
        let mut digest = true;
        for val in self.by_ref() {
            digest &= predicate(val)?;
            if !digest {
                break;
            }
        }
        Ok(digest)
    }

    /// This method functions similarly to `Iterator::any` but where the given predicate returns
    /// `Result`s.
    ///
    /// Like `Iterator::any`, this function short-circuits but will short-circuit if the predicate
    /// returns anything other than `Ok(false)`. If the first item that is not `Ok(false)` is
    /// `Ok(true)`, the returned value will be `Ok(true)`. If that item is `Err`, than that `Err`
    /// is returned.
    ///
    /// ```ignore
    /// // A totally accurate prime checker
    /// fn is_prime(i: usize) -> Result<bool, ()> {
    ///   match i {
    ///     0 | 1 => Err(()), // 0 and 1 are neither prime or composite
    ///     2 | 3 => Ok(true),
    ///     _ => Ok(false), // Every other number is composite, I guess
    ///   }
    /// }
    ///
    /// assert_eq!(Ok(false), [].into_iter().fallible_any(is_prime));
    /// assert_eq!(Ok(true), (2..5).fallible_any(is_prime));
    /// assert_eq!(Ok(false), (4..5).fallible_any(is_prime));
    /// assert_eq!(Err(()), (1..4).fallible_any(is_prime));
    /// assert_eq!(Err(()), (1..5).fallible_any(is_prime));
    /// ```
    fn fallible_any<E, F>(&mut self, mut predicate: F) -> Result<bool, E>
    where
        F: FnMut(Self::Item) -> Result<bool, E>,
    {
        let mut digest = false;
        for val in self.by_ref() {
            digest |= predicate(val)?;
            if digest {
                break;
            }
        }
        Ok(digest)
    }

    /// This method functions similarly to `FallibleIterator::fallible_any` but inverted. The
    /// existing iterator yields `Result`s but the predicate is not fallible.
    ///
    /// Like `FallibleIterator::fallible_any`, this function short-circuits but will short-circuit
    /// if it encounters an `Err` or `true`. If the existing iterator yields an `Err`, this
    /// function short-circuits, does not call the predicate, and returns that `Err`. If the value
    /// is `Ok`, it is given to the predicate. If the predicate returns `true`, this method returns
    /// `Ok(true)`.
    ///
    /// ```ignore
    /// let first_values = vec![];
    /// let second_values = vec![Ok(0), Err(())];
    /// let third_values = vec![Ok(1), Ok(3)];
    /// let fourth_values = vec![Err(()), Ok(0)];
    ///
    /// assert_eq!(Ok(false), first_values.into_iter().ok_and_any(is_even));
    /// assert_eq!(Ok(true), second_values.into_iter().ok_and_any(is_even));
    /// assert_eq!(Ok(false), third_values.into_iter().ok_and_any(is_even));
    /// assert_eq!(Err(()), fourth_values.into_iter().ok_and_any(is_even));
    /// ```
    fn ok_and_any<T, E, F>(&mut self, predicate: F) -> Result<bool, E>
    where
        Self: Iterator<Item = Result<T, E>>,
        F: FnMut(T) -> bool,
    {
        self.process_results(|mut results| results.any(predicate))
    }

    /// A convenience method that is equivalent to calling `.map(|result|
    /// result.and_then(fallible_fn))`.
    fn and_then<T, E, U, F>(self, map: F) -> AndThen<Self, F>
    where
        Self: Iterator<Item = Result<T, E>>,
        F: FnMut(T) -> Result<U, E>,
    {
        AndThen { iter: self, map }
    }

    /// Tries to find the first `Ok` value that matches the predicate. If an `Err` is found before
    /// the finding a match, the `Err` is returned.
    // NOTE: This is a nightly feature on `Iterator`. To avoid name collisions, this method is
    // named differently :(
    // Once stabilized, this method should probably be removed.
    fn find_ok<F, T, E>(&mut self, predicate: F) -> Result<Option<T>, E>
    where
        Self: Iterator<Item = Result<T, E>>,
        F: FnMut(&T) -> bool,
    {
        self.process_results(|mut results| results.find(predicate))
    }

    fn fallible_fold<F, O, T, E>(&mut self, mut init: O, mut mapper: F) -> Result<O, E>
    where
        Self: Iterator<Item = T>,
        F: FnMut(O, T) -> Result<O, E>,
    {
        for item in self {
            init = mapper(init, item)?;
        }
        Ok(init)
    }
}

impl<I: Itertools> FallibleIterator for I {}

/// The struct returned by [fallible_filter](FallibleIterator::fallible_filter).
pub(crate) struct FallibleFilter<I, F> {
    iter: I,
    predicate: F,
}

impl<I, F, E> Iterator for FallibleFilter<I, F>
where
    I: Iterator,
    F: FnMut(&I::Item) -> Result<bool, E>,
{
    type Item = Result<I::Item, E>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let val = self.iter.next()?;
            match (self.predicate)(&val) {
                Ok(true) => return Some(Ok(val)),
                Ok(false) => {}
                Err(e) => return Some(Err(e)),
            }
        }
    }
}

/// The struct returned by [and_then_filter](FallibleIterator::and_then_filter).
pub(crate) struct AndThenFilter<I, F> {
    iter: I,
    predicate: F,
}

impl<I, F, T, E> Iterator for AndThenFilter<I, F>
where
    I: Iterator<Item = Result<T, E>>,
    F: FnMut(&T) -> Result<bool, E>,
{
    type Item = Result<T, E>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let val = self.iter.next()?;
            return match val {
                Err(e) => Some(Err(e)),
                Ok(val) => match (self.predicate)(&val) {
                    Ok(true) => Some(Ok(val)),
                    Ok(false) => continue,
                    Err(e) => Some(Err(e)),
                },
            };
        }
    }
}

pub(crate) struct AndThen<I, F> {
    iter: I,
    map: F,
}

impl<I, T, E, U, F> Iterator for AndThen<I, F>
where
    I: Iterator<Item = Result<T, E>>,
    F: FnMut(T) -> Result<U, E>,
{
    type Item = Result<U, E>;

    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next().map(|res| res.and_then(&mut self.map))
    }
}

// Ideally, these would be doc tests, but gating access to the `utils` module is messy.
#[cfg(test)]
mod tests {
    use super::*;

    type Item = Result<usize, ()>;

    fn is_prime(i: usize) -> Result<bool, ()> {
        match i {
            0 | 1 => Err(()), // 0 and 1 are neither prime or composite
            2 | 3 => Ok(true),
            _ => Ok(false), // Every other number is composite, I guess
        }
    }

    fn is_even(i: usize) -> bool {
        i % 2 == 0
    }

    #[test]
    fn test_fallible_filter() {
        let vals = (1..6).fallible_filter(|i| is_prime(*i));
        itertools::assert_equal(vals, vec![Err(()), Ok(2), Ok(3)]);
    }

    #[test]
    fn test_and_then_filter() {
        let vals = vec![Ok(0), Err(()), Err(()), Ok(3), Ok(4)]
            .into_iter()
            .and_then_filter(|i| is_prime(*i));
        itertools::assert_equal(vals, vec![Err(()), Err(()), Err(()), Ok(3)]);
    }

    #[test]
    fn test_fallible_all() {
        assert_eq!(Ok(true), [].into_iter().fallible_all(is_prime));
        assert_eq!(Ok(true), (2..4).fallible_all(is_prime));
        assert_eq!(Err(()), (1..4).fallible_all(is_prime));
        assert_eq!(Ok(false), (2..5).fallible_all(is_prime));
        assert_eq!(Err(()), (1..5).fallible_all(is_prime));
    }

    #[test]
    fn test_fallible_any() {
        assert_eq!(Ok(false), [].into_iter().fallible_any(is_prime));
        assert_eq!(Ok(true), (2..5).fallible_any(is_prime));
        assert_eq!(Ok(false), (4..5).fallible_any(is_prime));
        assert_eq!(Err(()), (1..4).fallible_any(is_prime));
        assert_eq!(Err(()), (1..5).fallible_any(is_prime));
    }

    #[test]
    fn test_ok_and_any() {
        let first_values: Vec<Item> = vec![];
        let second_values: Vec<Item> = vec![Ok(0), Err(())];
        let third_values: Vec<Item> = vec![Ok(1), Ok(3)];
        let fourth_values: Vec<Item> = vec![Err(()), Ok(0)];

        assert_eq!(Ok(false), first_values.into_iter().ok_and_any(is_even));
        assert_eq!(Ok(true), second_values.into_iter().ok_and_any(is_even));
        assert_eq!(Ok(false), third_values.into_iter().ok_and_any(is_even));
        assert_eq!(Err(()), fourth_values.into_iter().ok_and_any(is_even));
    }
}
