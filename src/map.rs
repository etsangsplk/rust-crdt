use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;

use serde::Serialize;
use serde::de::DeserializeOwned;

use traits::{Causal, CvRDT, CmRDT};
use vclock::{Dot, VClock, Actor};
use ctx::{ReadCtx, AddCtx, RmCtx};

/// Key Trait alias to reduce redundancy in type decl.
pub trait Key: Debug + Ord + Clone + Send + Serialize + DeserializeOwned {}
impl<T: Debug + Ord + Clone + Send + Serialize + DeserializeOwned> Key for T {}

/// Val Trait alias to reduce redundancy in type decl.
pub trait Val<A: Actor>
    : Debug + Default + Clone + Send + Serialize + DeserializeOwned
    + Causal<A> + CmRDT + CvRDT
{}

impl<A, T> Val<A> for T where
    A: Actor,
    T: Debug + Default + Clone + Send + Serialize + DeserializeOwned
    + Causal<A> + CmRDT + CvRDT
{}

/// Map CRDT - Supports Composition of CRDT's with reset-remove semantics.
///
/// Reset-remove means that if one replica removes an entry while another
/// actor concurrently edits that entry, once we sync these two maps, we
/// will see that the entry is still in the map but all edits seen by the
/// removing actor will be gone. To understand this more clearly see the
/// following example:
///
/// ``` rust
/// use crdts::{Map, Orswot, CvRDT, CmRDT};
///
/// type Actor = u64;
/// type Friend = String;
///
/// let mut friends: Map<Friend, Orswot<Friend, Actor>, Actor> = Map::new();
/// let a1 = 10837103590u64; // initial actors id
///
/// let op = friends.update(
///     "alice",
///     friends.get(&"alice".to_string()).derive_add_ctx(a1),
///     |set, ctx| set.add("bob", ctx)
/// );
/// friends.apply(&op);
///
/// let mut friends_replica = friends.clone();
/// let a2 = 8947212u64; // the replica's actor id
///
/// // now the two maps diverge. the original map will remove "alice" from the map
/// // while the replica map will update the "alice" friend set to include "clyde".
///
/// let rm_op = friends.rm("alice", friends.get(&"alice".to_string()).derive_rm_ctx());
/// friends.apply(&rm_op);
///
/// let replica_op = friends_replica.update(
///     "alice",
///     friends_replica.get(&"alice".into()).derive_add_ctx(a2),
///     |set, ctx| set.add("clyde", ctx)
/// );
/// friends_replica.apply(&replica_op);
///
/// assert_eq!(friends.get(&"alice".into()).val, None);
/// assert_eq!(
///     friends_replica.get(&"alice".into()).val.map(|set| set.value().val),
///     Some(vec!["bob".to_string(), "clyde".to_string()].into_iter().collect())
/// );
///
/// // On merge, we should see "alice" in the map but her friend set should only have "clyde".
///
/// friends.merge(&friends_replica);
///
/// let alice_friends = friends.get(&"alice".into()).val
///     .map(|set| set.value().val);
/// assert_eq!(alice_friends, Some(vec!["clyde".into()].into_iter().collect()));
/// ```
#[serde(bound(deserialize = ""))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Map<K: Key, V: Val<A>, A: Actor> {
    // This clock stores the current version of the Map, it should
    // be greator or equal to all Entry.clock's in the Map.
    clock: VClock<A>,
    entries: BTreeMap<K, Entry<V, A>>,
    deferred: HashMap<VClock<A>, BTreeSet<K>>
}

#[serde(bound(deserialize = ""))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Entry<V: Val<A>, A: Actor> {
    // The entry clock tells us which actors edited this entry.
    clock: VClock<A>,

    // The nested CRDT
    val: V
}

/// Operations which can be applied to the Map CRDT
#[serde(bound(deserialize = ""))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op<K: Key, V: Val<A>, A: Actor> {
    /// No change to the CRDT
    Nop,
    /// Remove a key from the map
    Rm {
        /// The clock under which we will perform this remove
        clock: VClock<A>,
        /// Key to remove
        key: K
    },
    /// Update an entry in the map
    Up {
        /// Actors version at the time of the update
        dot: Dot<A>,
        /// Key of the value to update
        key: K,
        /// The operation to apply on the value under `key`
        op: V::Op
    }
}

impl<K: Key, V: Val<A>, A: Actor> Default for Map<K, V, A> {
    fn default() -> Self {
        Map::new()
    }
}

impl<K: Key, V: Val<A>, A: Actor> Causal<A> for Map<K, V, A> {
    fn truncate(&mut self, clock: &VClock<A>) {
        let mut to_remove: Vec<K> = Vec::new();
        for (key, entry) in self.entries.iter_mut() {
            entry.clock.subtract(&clock);
            if entry.clock.is_empty() {
                to_remove.push(key.clone());
            } else {
                entry.val.truncate(&clock);
            }
        }

        for key in to_remove {
            self.entries.remove(&key);
        }

        let mut deferred = HashMap::new();
        for (mut rm_clock, key) in self.deferred.clone().into_iter() {
            rm_clock.subtract(&clock);
            if !rm_clock.is_empty() {
                deferred.insert(rm_clock, key);
            }
        }
        self.deferred = deferred;

        self.clock.subtract(&clock);
    }
}

impl<K: Key, V: Val<A>, A: Actor> CmRDT for Map<K, V, A> {
    type Op = Op<K, V, A>;

    fn apply(&mut self, op: &Self::Op) {
        match op.clone() {
            Op::Nop => {/* do nothing */},
            Op::Rm { clock, key } => {
                self.apply_rm(key, &clock);
            },
            Op::Up { dot: Dot { actor, counter }, key, op } => {
                if self.clock.get(&actor) >= counter {
                    // we've seen this op already
                    return;
                }

                let mut entry = self.entries.remove(&key)
                    .unwrap_or_else(|| Entry {
                        clock: VClock::new(),
                        val: V::default()
                    });

                entry.clock.witness(actor.clone(), counter);
                entry.val.apply(&op);
                self.entries.insert(key.clone(), entry);

                self.clock.witness(actor, counter);
                self.apply_deferred();
            }
        }
    }
}

impl<K: Key, V: Val<A>, A: Actor> CvRDT for Map<K, V, A> {
    fn merge(&mut self, other: &Self) {
        let mut other_remaining = other.entries.clone();
        let mut keep = BTreeMap::new();
        for (key, mut entry) in self.entries.clone().into_iter() {
            match other.entries.get(&key).cloned() {
                None => {
                    // other doesn't contain this entry because it:
                    //  1. has witnessed it and dropped it
                    //  2. hasn't witnessed it
                    entry.clock.subtract(&other.clock);
                    if entry.clock.is_empty() {
                        // other has seen this entry and dropped it
                    } else {
                        // the other map has not seen this entry, so add it
                        let mut actors_who_have_deleted_this_entry = other.clock.clone();
                        actors_who_have_deleted_this_entry.subtract(&entry.clock);
                        entry.val.truncate(&actors_who_have_deleted_this_entry);
                        keep.insert(key, entry);
                    }
                }
                Some(mut other_entry) => {
                    // SUBTLE: this entry is present in both orswots, BUT that doesn't mean we
                    // shouldn't drop it!
                    let common = entry.clock.intersection(&other_entry.clock);
                    entry.clock.subtract(&common);
                    other_entry.clock.subtract(&common);
                    entry.clock.subtract(&other.clock);
                    other_entry.clock.subtract(&self.clock);

                    // Perfectly possible that an item in both sets should be dropped
                    let mut common = common;
                    common.merge(&entry.clock);
                    common.merge(&other_entry.clock);

                    if !common.is_empty() {
                        // we should not drop, as there are common clocks
                        entry.val.merge(&other_entry.val);
                        let mut actors_who_have_deleted_this_entry = entry.clock.clone();
                        actors_who_have_deleted_this_entry.merge(&other_entry.clock);
                        actors_who_have_deleted_this_entry.subtract(&common);

                        entry.val.truncate(&actors_who_have_deleted_this_entry);
                        entry.clock = common;
                        keep.insert(key.clone(), entry);
                    }
                    // don't want to consider this again below
                    other_remaining.remove(&key).unwrap();
                }
            }
        }

        for (key, mut entry) in other_remaining.into_iter() {
            entry.clock.subtract(&self.clock);
            if !entry.clock.is_empty() {
                // other has witnessed a novel addition, so add it
                let mut actors_who_deleted_this_entry = self.clock.clone();
                actors_who_deleted_this_entry.subtract(&entry.clock);
                entry.val.truncate(&actors_who_deleted_this_entry);
                keep.insert(key, entry);
            }
        }

        // merge deferred removals
        for (clock, deferred) in other.deferred.iter() {
            for key in deferred {
                self.apply_rm(key.clone(), &clock);
            }
        }

        self.entries = keep;

        // merge vclocks
        self.clock.merge(&other.clock);

        self.apply_deferred();
    }
}

impl<K: Key, V: Val<A>, A: Actor> Map<K, V, A> {
    /// Constructs an empty Map
    pub fn new() -> Map<K, V, A> {
        Map {
            clock: VClock::new(),
            entries: BTreeMap::new(),
            deferred: HashMap::new()
         }
    }

    /// Returns the number of entries in the Map
    pub fn len(&self) -> ReadCtx<usize, A> {
        ReadCtx {
            add_clock: self.clock.clone(),
            rm_clock: self.clock.clone(),
            val: self.entries.len()
        }
    }

    /// Retrieve value stored under a key
    pub fn get(&self, key: &K) -> ReadCtx<Option<V>, A> {
        let add_clock = self.clock.clone();
        let entry_opt = self.entries.get(&key);
        ReadCtx {
            add_clock: add_clock,
            rm_clock: entry_opt
                .map(|map_entry| map_entry.clock.clone())
                .unwrap_or_else(|| VClock::new()),
            val: entry_opt
                .map(|map_entry| map_entry.val.clone())
        }
    }

    /// Update a value under some key, if the key is not present in the map,
    /// the updater will be given the result of V::default().
    pub fn update<F, I>(&self, key: I, ctx: AddCtx<A>, f: F) -> Op<K, V, A>
        where F: FnOnce(&V, AddCtx<A>) -> V::Op,
              I: Into<K>
    {
        let key = key.into();
        let op = if let Some(entry) = self.entries.get(&key) {
            f(&entry.val, ctx.clone())
        } else {
            f(&V::default(), ctx.clone())
        };
        Op::Up { dot: ctx.dot, key, op }
    }

    /// Remove an entry from the Map
    pub fn rm(&self, key: impl Into<K>, ctx: RmCtx<A>) -> Op<K, V, A> {
        Op::Rm { clock: ctx.clock, key: key.into() }
    }

    /// apply the pending deferred removes 
    fn apply_deferred(&mut self) {
        let deferred = self.deferred.clone();
        self.deferred = HashMap::new();
        for (clock, keys) in deferred {
            for key in keys {
                self.apply_rm(key, &clock);
            }
        }
    }

    /// Apply a key removal given a clock.
    fn apply_rm(&mut self, key: K, clock: &VClock<A>) {
        if !(clock <= &self.clock) {
            let deferred_set = self.deferred.entry(clock.clone())
                .or_insert_with(|| BTreeSet::new());
            deferred_set.insert(key.clone());
        }

        if let Some(mut existing_entry) = self.entries.remove(&key) {
            existing_entry.clock.subtract(&clock);
            if !existing_entry.clock.is_empty() {
                existing_entry.val.truncate(&clock);
                self.entries.insert(key.clone(), existing_entry);
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use mvreg::{self, MVReg};

    type TestActor = u8;
    type TestKey = u8;
    type TestVal = MVReg<u8, TestActor>;
    type TestMap =  Map<TestKey, Map<TestKey, TestVal, TestActor>, TestActor>;

    #[test]
    fn test_get() {
        let mut m: TestMap = Map::new();

        assert_eq!(m.get(&0).val, None);

        let op_1 = m.clock.inc(1);
        m.clock.apply(&op_1);

        m.entries.insert(0, Entry {
            clock: m.clock.clone(),
            val: Map::default()
        });

        assert_eq!(m.get(&0).val, Some(Map::new()));
    }
    
    #[test]
    fn test_op_exchange_converges_quickcheck1() {
        let op_actor1 = Op::Up {
            dot: Dot { actor: 0, counter: 3 },
            key: 9,
            op: Op::Up {
                dot: Dot { actor: 0, counter: 3 },
                key: 0,
                op: mvreg::Op::Put {
                    clock: Dot { actor: 0, counter: 3 }.into(),
                    val: 0
                }
            }
        };
        let op_1_actor2 = Op::Up {
            dot: Dot { actor: 1, counter: 1 },
            key: 9,
            op: Op::Rm {
                clock: Dot { actor: 1, counter: 1 }.into(),
                key: 0
            }
        };
        let op_2_actor2 = Op::Rm {
            clock: Dot { actor: 1, counter: 2 }.into(),
            key: 9
        };
        
        let mut m1: TestMap = Map::new();
        let mut m2: TestMap = Map::new();

        m1.apply(&op_actor1);
        assert_eq!(m1.clock, Dot { actor: 0, counter: 3 }.into());
        assert_eq!(m1.entries.get(&9).unwrap().clock, Dot { actor: 0, counter: 3 }.into());
        assert_eq!(m1.entries.get(&9).unwrap().val.deferred.len(), 0);

        m2.apply(&op_1_actor2);
        m2.apply(&op_2_actor2);
        assert_eq!(m2.clock, Dot { actor: 1, counter: 1 }.into());
        assert_eq!(m2.entries.get(&9), None);
        assert_eq!(
            m2.deferred.get(&Dot { actor: 1, counter: 2 }.into()),
            Some(&vec![9].into_iter().collect())
        );
        
        // m1 <- m2
        m1.apply(&op_1_actor2);
        m1.apply(&op_2_actor2);
        
        // m2 <- m1
        m2.apply(&op_actor1);
        
        // m1 <- m2 == m2 <- m1
        assert_eq!(m1, m2);
    }
}
