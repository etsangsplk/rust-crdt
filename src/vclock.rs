//! The `vclock` crate provides a generic vector clock implementation.
//!
//! # Examples
//!
//! ```
//! use crdts::VClock;
//! let (mut a, mut b) = (VClock::new(), VClock::new());
//! a.witness("A".to_string(), 2);
//! b.witness("A".to_string(), 1);
//! assert!(a > b);
//! ```

// TODO: we have a mixture of language here with witness and actor. Clean this up

use super::*;

use std::fmt::{self, Debug, Display};
use std::cmp::{self, Ordering};
use std::collections::{BTreeMap, btree_map};
use std::hash::Hash;

/// A counter is used to track causality at a particular actor.
pub type Counter = u64;

/// Common Actor type, Actors are unique identifier for every `thing` mutating a VClock.
/// VClock based CRDT's will need to expose this Actor type to the user.
pub trait Actor: Ord + Clone + Hash + Send + Serialize + DeserializeOwned + Debug {}
impl<A: Ord + Clone + Hash + Send + Serialize + DeserializeOwned + Debug> Actor for A {}


/// Dot is a version marker for a single actor
#[serde(bound(deserialize = ""))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dot<A: Actor> {
    /// The actor identifier
    pub actor: A,
    /// The current version of this actor
    pub counter: Counter
}

// TODO: VClock derives an Ord implementation, but a total order over VClocks doesn't exist. I think this may mess up our BTreeMap usage in ORSWOT and friends

/// A `VClock` is a standard vector clock.
/// It contains a set of "actors" and associated counters.
/// When a particular actor witnesses a mutation, their associated
/// counter in a `VClock` is incremented. `VClock` is typically used
/// as metadata for associated application data, rather than as the
/// container for application data. `VClock` just tracks causality.
/// It can tell you if something causally descends something else,
/// or if different replicas are "concurrent" (were mutated in
/// isolation, and need to be resolved externally).
#[serde(bound(deserialize = ""))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VClock<A: Actor> {
    /// dots is the mapping from actors to their associated counters
    pub dots: BTreeMap<A, Counter>,
}

impl<A: Actor> PartialOrd for VClock<A> {
    fn partial_cmp(&self, other: &VClock<A>) -> Option<Ordering> {
        if self == other {
            Some(Ordering::Equal)
        } else if other.dots.iter().all(|(w, c)| &self.get(w) >= c) {
            Some(Ordering::Greater)
        } else if self.dots.iter().all(|(w, c)| &other.get(w) >= c) {
            Some(Ordering::Less)
        } else {
            None
        }
    }
}

impl<A: Actor + Display> Display for VClock<A> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "(")?;
        for (i, (actor, count)) in self.dots.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}->{}", actor, count)?;
        }
        write!(f, ")")
    }
}

impl<A: Actor> Causal<A> for VClock<A> {
    /// Truncates to the greatest-lower-bound of the given VClock and self
    /// ``` rust
    /// use crdts::{VClock, Causal};
    /// let mut c = VClock::new();
    /// c.witness(23, 6);
    /// c.witness(89, 14);
    /// let c2 = c.clone();
    ///
    /// c.truncate(&c2); // should be a no-op
    /// assert_eq!(c, c2);
    ///
    /// c.witness(43, 1);
    /// assert_eq!(c.get(&43), 1);
    /// c.truncate(&c2); // should remove the 43 => 1 entry
    /// assert_eq!(c.get(&43), 0);
    /// ```
    fn truncate(&mut self, other: &VClock<A>) {
        let mut actors_to_remove: Vec<A> = Vec::new();
        for (actor, count) in self.dots.iter_mut() {
            let min_count = cmp::min(*count, other.get(actor));
            if min_count > 0 {
                *count = min_count
            } else {
                // Since an actor missing from the dots map has an implied counter of 0
                // we can save some memory, and remove the actor.
                actors_to_remove.push(actor.clone())
            }
        }

        // finally, remove all the zero counter actor
        for actor in actors_to_remove {
            self.dots.remove(&actor);
        }
    }
}

impl<A: Actor> CmRDT for VClock<A> {
    type Op = Dot<A>;

    fn apply(&mut self, dot: &Self::Op) {
        let _ = self.witness(dot.actor.clone(), dot.counter);
    }
}

impl<A: Actor> CvRDT for VClock<A> {
    fn merge(&mut self, other: &VClock<A>) {
        for (actor, counter) in other.dots.iter() {
            let _ = self.witness(actor.clone(), *counter);
        }
    }
}

impl<A: Actor> VClock<A> {
    /// Returns a new `VClock` instance.
    pub fn new() -> VClock<A> {
        VClock { dots: BTreeMap::new() }
    }

    /// For a particular actor, possibly store a new counter
    /// if it dominates.
    ///
    /// # Examples
    ///
    /// ```
    /// use crdts::VClock;
    /// let (mut a, mut b) = (VClock::new(), VClock::new());
    /// a.witness("A".to_string(), 2);
    /// a.witness("A".to_string(), 0); // ignored because 2 dominates 0
    /// b.witness("A".to_string(), 1);
    /// assert!(a > b);
    /// ```
    ///
    pub fn witness(&mut self, actor: A, counter: Counter) {
        if !(self.get(&actor) >= counter) {
            self.dots.insert(actor, counter);
        }
    }

    /// For a particular actor, increment the associated counter.
    ///
    /// # Examples
    ///
    /// ```
    /// use crdts::{VClock, CmRDT};
    /// let (mut a, mut b) = (VClock::new(), VClock::new());
    /// let a_op1 = a.inc("A".to_string());
    /// a.apply(&a_op1);
    /// let a_op2 = a.inc("A".to_string());
    /// a.apply(&a_op2);
    ///
    /// a.witness("A".to_string(), 0); // ignored because 2 dominates 0
    /// let b_op = b.inc("A".to_string());
    /// b.apply(&b_op);
    /// assert!(a > b);
    /// ```
    pub fn inc(&self, actor: A) -> Dot<A> {
        let next = self.get(&actor) + 1;
        Dot { actor: actor, counter: next }
    }

    /// True if two vector clocks have diverged.
    ///
    /// # Examples
    ///
    /// ```
    /// use crdts::{VClock, CmRDT};
    /// let (mut a, mut b) = (VClock::new(), VClock::new());
    /// let a_op = a.inc("A".to_string());
    /// a.apply(&a_op);
    /// let b_op = b.inc("B".to_string());
    /// b.apply(&b_op);
    /// assert!(a.concurrent(&b));
    /// ```
    pub fn concurrent(&self, other: &VClock<A>) -> bool {
        self.partial_cmp(other).is_none()
    }

    /// Return the associated counter for this actor.
    /// All actors not in the vclock have an implied count of 0
    pub fn get(&self, actor: &A) -> Counter {
        self.dots.get(actor)
            .map(|counter| *counter)
            .unwrap_or(0)
    }

    /// Returns `true` if this vector clock contains nothing.
    pub fn is_empty(&self) -> bool {
        self.dots.is_empty()
    }

    /// Returns the common elements (same actor and counter)
    /// for two `VClock` instances.
    pub fn intersection(&self, other: &VClock<A>) -> VClock<A> {
        let mut dots = BTreeMap::new();
        for (actor, counter) in self.dots.iter() {
            let other_counter = other.get(actor);
            if other_counter == *counter {
                dots.insert(actor.clone(), *counter);
            }
        }
        VClock { dots: dots }
    }

    /// Returns an iterator over the dots in this vclock
    pub fn iter(&self) -> impl Iterator<Item=(&A, &u64)> {
        self.dots.iter()
    }

    /// Forget actors who appear in the given VClock with descendent dots
    pub fn subtract(&mut self, other: &VClock<A>) {
        for (actor, counter) in other.iter() {
            if counter >= &self.get(&actor) {
                self.dots.remove(&actor);
            }
        }
    }
}

impl<A: Actor> std::iter::IntoIterator for VClock<A> {
    type Item = (A, u64);
    type IntoIter = btree_map::IntoIter<A, u64>;
    
    /// Consumes the vclock and returns an iterator over dots in the clock
    fn into_iter(self) -> btree_map::IntoIter<A, u64> {
        self.dots.into_iter()
    }
}

impl<A: Actor> std::iter::FromIterator<(A, u64)> for VClock<A> {
    fn from_iter<I: IntoIterator<Item=(A, u64)>>(iter: I) -> Self {
        let mut clock = Self::new();

        for (actor, counter) in iter {
            let _ = clock.witness(actor, counter);
        }

        clock
    }
}

impl<A: Actor> From<Vec<(A, u64)>> for VClock<A> {
    fn from(vec: Vec<(A, u64)>) -> Self {
        vec.into_iter().collect()
    }
}

impl<A: Actor> From<Dot<A>> for VClock<A> {
    fn from(dot: Dot<A>) -> Self {
        let mut clock = VClock::new();
        clock.witness(dot.actor, dot.counter);
        clock
    }
}
