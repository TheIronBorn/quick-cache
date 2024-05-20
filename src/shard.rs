use std::{
    hash::{BuildHasher, Hash},
    mem::{self, MaybeUninit},
};

use hashbrown::raw::RawTable;

use crate::{
    linked_slab::{LinkedSlab, Token},
    shim::sync::atomic::{self, AtomicU16},
    Equivalent, Lifecycle, Weighter,
};

#[cfg(feature = "stats")]
use crate::shim::sync::atomic::AtomicU64;

// Max reference counter, this is 1 in ClockPro and 3 in the S3-FIFO.
const MAX_F: u16 = 2;

pub trait SharedPlaceholder: Clone {
    fn new(hash: u64, idx: Token) -> Self;
    fn same_as(&self, other: &Self) -> bool;
    fn hash(&self) -> u64;
    fn idx(&self) -> Token;
}

pub enum InsertStrategy {
    Insert,
    Replace { soft: bool },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ResidentState {
    Hot,
    Cold,
}

#[derive(Debug)]
pub struct Resident<Key, Val> {
    key: Key,
    value: Val,
    state: ResidentState,
    referenced: AtomicU16,
}

#[derive(Debug)]
struct Placeholder<Key, Plh> {
    key: Key,
    hot: ResidentState,
    shared: Plh,
}

enum Entry<Key, Val, Plh> {
    Resident(Resident<Key, Val>),
    Placeholder(Placeholder<Key, Plh>),
    Ghost(u64),
}

/// A bounded cache using a modified CLOCK-PRO eviction policy.
/// The implementation allows some parallelism as gets don't require exclusive access.
/// Any evicted items are returned so they can be dropped by the caller, outside the locks.
pub struct CacheShard<Key, Val, We, B, L, Plh> {
    hash_builder: B,
    /// Map to an entry in the `entries` slab.
    /// Note that the actual key/value/hash are not stored in the map but in the slab.
    map: RawTable<Token>,
    /// Slab holding entries
    entries: LinkedSlab<Entry<Key, Val, Plh>>,
    /// Head of cold list, containing Cold entries.
    cold_head: Option<Token>,
    /// Head of hot list, containing Hot entries.
    hot_head: Option<Token>,
    /// Head of ghost list, containing non-resident/Hash entries.
    ghost_head: Option<Token>,
    weight_target_hot: u64,
    weight_capacity: u64,
    weight_hot: u64,
    weight_cold: u64,
    num_hot: usize,
    num_cold: usize,
    num_non_resident: usize,
    capacity_non_resident: usize,
    #[cfg(feature = "stats")]
    hits: AtomicU64,
    #[cfg(feature = "stats")]
    misses: AtomicU64,
    weighter: We,
    pub(crate) lifecycle: L,
}

#[cfg(feature = "stats")]
macro_rules! record_hit {
    ($self: expr) => {{
        $self.hits.fetch_add(1, atomic::Ordering::Relaxed);
    }};
}
#[cfg(feature = "stats")]
macro_rules! record_hit_mut {
    ($self: expr) => {{
        *$self.hits.get_mut() += 1;
    }};
}
#[cfg(feature = "stats")]
macro_rules! record_miss {
    ($self: expr) => {{
        $self.misses.fetch_add(1, atomic::Ordering::Relaxed);
    }};
}
#[cfg(feature = "stats")]
macro_rules! record_miss_mut {
    ($self: expr) => {{
        *$self.misses.get_mut() += 1;
    }};
}

#[cfg(not(feature = "stats"))]
macro_rules! record_hit {
    ($self: expr) => {{}};
}
#[cfg(not(feature = "stats"))]
macro_rules! record_hit_mut {
    ($self: expr) => {{}};
}
#[cfg(not(feature = "stats"))]
macro_rules! record_miss {
    ($self: expr) => {{}};
}
#[cfg(not(feature = "stats"))]
macro_rules! record_miss_mut {
    ($self: expr) => {{}};
}

impl<Key, Val, We, B, L, Plh: SharedPlaceholder> CacheShard<Key, Val, We, B, L, Plh> {
    pub fn remove_placeholder(&mut self, placeholder: &Plh) {
        self.map.remove_entry(placeholder.hash(), |&idx| {
            if idx != placeholder.idx() {
                return false;
            }
            let (entry, _) = self.entries.get(idx).unwrap();
            matches!(entry, Entry::Placeholder(Placeholder { shared, .. }) if shared.same_as(placeholder))
        });
    }
}

impl<
        Key: Eq + Hash,
        Val,
        We: Weighter<Key, Val>,
        B: BuildHasher,
        L: Lifecycle<Key, Val>,
        Plh: SharedPlaceholder,
    > CacheShard<Key, Val, We, B, L, Plh>
{
    pub fn new(
        hot_allocation: f64,
        ghost_allocation: f64,
        estimated_items_capacity: usize,
        weight_capacity: u64,
        weighter: We,
        hash_builder: B,
        lifecycle: L,
    ) -> Self {
        let weight_target_hot = (weight_capacity as f64 * hot_allocation) as u64;
        let capacity_non_resident = (estimated_items_capacity as f64 * ghost_allocation) as usize;
        Self {
            hash_builder,
            map: RawTable::with_capacity(0),
            entries: LinkedSlab::with_capacity(0),
            weight_capacity,
            #[cfg(feature = "stats")]
            hits: Default::default(),
            #[cfg(feature = "stats")]
            misses: Default::default(),
            cold_head: None,
            hot_head: None,
            ghost_head: None,
            capacity_non_resident,
            weight_target_hot,
            num_hot: 0,
            num_cold: 0,
            num_non_resident: 0,
            weight_hot: 0,
            weight_cold: 0,
            weighter,
            lifecycle,
        }
    }

    #[cfg(fuzzing)]
    pub fn validate(&self) {
        self.entries.validate();
        let mut num_hot = 0;
        let mut num_cold = 0;
        let mut num_non_resident = 0;
        let mut weight_hot = 0;
        let mut weight_cold = 0;
        for e in self.entries.iter_entries() {
            match e {
                Entry::Resident(r) if r.state == ResidentState::Cold => {
                    num_cold += 1;
                    weight_cold += self.weighter.weight(&r.key, &r.value);
                }
                Entry::Resident(r) => {
                    num_hot += 1;
                    weight_hot += self.weighter.weight(&r.key, &r.value);
                }
                Entry::Ghost(_) => {
                    num_non_resident += 1;
                }
                Entry::Placeholder(_) => (),
            }
        }
        // eprintln!("-------------");
        // dbg!(num_hot, num_cold, num_non_resident, weight_hot, weight_cold);
        // dbg!(
        //     self.num_hot,
        //     self.num_cold,
        //     self.num_non_resident,
        //     self.weight_hot,
        //     self.weight_cold,
        //     self.weight_target_hot,
        //     self.capacity_non_resident
        // );
        assert_eq!(num_hot, self.num_hot);
        assert_eq!(num_cold, self.num_cold);
        assert_eq!(num_non_resident, self.num_non_resident);
        assert_eq!(weight_hot, self.weight_hot);
        assert_eq!(weight_cold, self.weight_cold);
        assert!(weight_hot + weight_cold <= self.weight_capacity);
        assert!(num_non_resident <= self.capacity_non_resident);
    }

    /// Reserver additional space for `additional` entries.
    /// Note that this is counted in entries, and is not weighted.
    pub fn reserve(&mut self, additional: usize) {
        // extra 50% for non-resident entries
        let additional = additional.saturating_add(additional / 2);
        self.map.reserve(additional, |&idx| {
            let (entry, _) = self.entries.get(idx).unwrap();
            match entry {
                Entry::Resident(Resident { key, .. })
                | Entry::Placeholder(Placeholder { key, .. }) => {
                    Self::hash_static(&self.hash_builder, key)
                }
                Entry::Ghost(non_resident_hash) => *non_resident_hash,
            }
        })
    }

    pub fn weight(&self) -> u64 {
        self.weight_hot + self.weight_cold
    }

    pub fn len(&self) -> usize {
        self.num_hot + self.num_cold
    }

    pub fn capacity(&self) -> u64 {
        self.weight_capacity
    }

    #[cfg(feature = "stats")]
    pub fn hits(&self) -> u64 {
        self.hits.load(atomic::Ordering::Relaxed)
    }

    #[cfg(feature = "stats")]
    pub fn misses(&self) -> u64 {
        self.misses.load(atomic::Ordering::Relaxed)
    }

    pub fn clear(&mut self) {
        let _ = self.drain();
    }

    pub fn drain(&mut self) -> impl Iterator<Item = (Key, Val)> + '_ {
        self.cold_head = None;
        self.hot_head = None;
        self.ghost_head = None;
        self.num_hot = 0;
        self.num_cold = 0;
        self.num_non_resident = 0;
        self.weight_hot = 0;
        self.weight_cold = 0;
        self.map.clear();
        self.entries.drain().filter_map(|i| match i {
            Entry::Resident(r) => Some((r.key, r.value)),
            Entry::Placeholder(_) | Entry::Ghost(_) => None,
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (&'_ Key, &'_ Val)> + '_ {
        self.entries.iter().filter_map(|i| match i {
            Entry::Resident(r) => Some((&r.key, &r.value)),
            Entry::Placeholder(_) | Entry::Ghost(_) => None,
        })
    }

    #[inline]
    fn hash_static<Q>(hasher: &B, key: &Q) -> u64
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        hasher.hash_one(key)
    }

    #[inline]
    pub fn hash<Q>(&self, key: &Q) -> u64
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        Self::hash_static(&self.hash_builder, key)
    }

    #[inline]
    fn search<Q>(&self, hash: u64, k: &Q) -> Option<Token>
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        // Safety for `RawTable::iter_hash` and `Bucket::as_ref`:
        // * Their outputs do not outlive their HashBrown:
        // The HashBrown instance is alive for the entirety of the function and
        // the content references never leave the function.
        // * HashBrown can't be mutated while being iterated with iter_hash:
        // The HashBrown instance isn't mutated in this method.
        unsafe {
            let mut hash_match = None;
            for bucket in self.map.iter_hash(hash) {
                let idx = *bucket.as_ref();
                let (entry, _) = self.entries.get(idx).unwrap();
                match entry {
                    Entry::Resident(Resident { key, .. })
                    | Entry::Placeholder(Placeholder { key, .. })
                        if k.equivalent(key) =>
                    {
                        return Some(idx);
                    }
                    Entry::Ghost(non_resident_hash) if *non_resident_hash == hash => {
                        hash_match = Some(idx);
                    }
                    _ => (),
                }
            }
            hash_match
        }
    }

    #[inline]
    fn search_resident<Q>(&self, hash: u64, k: &Q) -> Option<(Token, &Resident<Key, Val>)>
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        let mut resident = MaybeUninit::uninit();
        self.map
            .get(hash, |&idx| {
                let (entry, _) = self.entries.get(idx).unwrap();
                match entry {
                    Entry::Resident(r) if k.equivalent(&r.key) => {
                        resident.write(r);
                        true
                    }
                    _ => false,
                }
            })
            .map(|idx| {
                // Safety: we found a successful entry in `get` above,
                // thus resident is initialized.
                (*idx, unsafe { resident.assume_init() })
            })
    }

    pub fn get<Q>(&self, hash: u64, key: &Q) -> Option<&Val>
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        if let Some((_, resident)) = self.search_resident(hash, key) {
            let referenced = resident.referenced.load(atomic::Ordering::Relaxed);
            // Attempt to avoid contention on hot items, which may have their counters maxed out
            if referenced < MAX_F {
                // While extremely unlikely, this increment can theoretically overflow.
                // Even if that happens there's no impact correctness wise.
                resident.referenced.fetch_add(1, atomic::Ordering::Relaxed);
            }
            record_hit!(self);
            Some(&resident.value)
        } else {
            record_miss!(self);
            None
        }
    }

    pub fn get_mut<Q>(&mut self, hash: u64, key: &Q) -> Option<RefMut<'_, Key, Val, We>>
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        let Some((idx, _)) = self.search_resident(hash, key) else {
            record_miss_mut!(self);
            return None;
        };
        let Some((Entry::Resident(resident), _)) = self.entries.get_mut(idx) else {
            unreachable!()
        };
        if *resident.referenced.get_mut() < MAX_F {
            *resident.referenced.get_mut() += 1;
        }
        record_hit_mut!(self);

        let weight = if resident.state == ResidentState::Hot {
            &mut self.weight_hot
        } else {
            &mut self.weight_cold
        };
        *weight -= self.weighter.weight(&resident.key, &resident.value);
        Some(RefMut {
            key: &resident.key,
            value: &mut resident.value,
            weight,
            weighter: &self.weighter,
        })
    }

    #[inline]
    pub fn peek_token(&self, token: Token) -> Option<&Val> {
        if let Some((Entry::Resident(resident), _)) = self.entries.get(token) {
            Some(&resident.value)
        } else {
            None
        }
    }

    pub fn peek_token_mut(&mut self, token: Token) -> Option<RefMut<'_, Key, Val, We>> {
        if let Some((Entry::Resident(resident), _)) = self.entries.get_mut(token) {
            let weight = if resident.state == ResidentState::Hot {
                &mut self.weight_hot
            } else {
                &mut self.weight_cold
            };
            *weight -= self.weighter.weight(&resident.key, &resident.value);
            Some(RefMut {
                key: &resident.key,
                value: &mut resident.value,
                weight,
                weighter: &self.weighter,
            })
        } else {
            None
        }
    }

    pub fn peek<Q>(&self, hash: u64, key: &Q) -> Option<&Val>
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        let (_, resident) = self.search_resident(hash, key)?;
        Some(&resident.value)
    }

    pub fn peek_mut<Q>(&mut self, hash: u64, key: &Q) -> Option<RefMut<'_, Key, Val, We>>
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        let (idx, _) = self.search_resident(hash, key)?;
        self.peek_token_mut(idx)
    }

    pub fn remove<Q>(&mut self, hash: u64, key: &Q) -> Option<(Key, Val)>
    where
        Q: Hash + Equivalent<Key> + ?Sized,
    {
        // This could be search_resident, but calling `remove` is likely a
        // strong indication that this input isn't important.
        let idx = self.search(hash, key)?;
        self.remove_internal(hash, idx)
    }

    fn remove_internal(&mut self, hash: u64, idx: Token) -> Option<(Key, Val)> {
        self.map_remove(hash, idx);
        let mut result = None;
        let (entry, next) = self.entries.remove(idx).unwrap();
        let list_head = match entry {
            Entry::Resident(r) => {
                let weight = self.weighter.weight(&r.key, &r.value);
                result = Some((r.key, r.value));
                if r.state == ResidentState::Hot {
                    self.num_hot -= 1;
                    self.weight_hot -= weight;
                    &mut self.hot_head
                } else {
                    debug_assert!(r.state == ResidentState::Cold);
                    self.num_cold -= 1;
                    self.weight_cold -= weight;
                    &mut self.cold_head
                }
            }
            Entry::Ghost(_) => {
                // Since this an user invoked remove we opt to remove even Ghost entries that could match it.
                self.num_non_resident -= 1;
                &mut self.ghost_head
            }
            Entry::Placeholder(_) => {
                // TODO: this is probably undesirable as it could lead to two placeholders for the same key.
                return None;
            }
        };
        if *list_head == Some(idx) {
            *list_head = next;
        }
        result
    }

    /// Advance cold ring, promoting to hot and demoting as needed.
    /// Panics if the cache is empty.
    fn advance_cold(&mut self, lcs: &mut L::RequestState) {
        debug_assert_ne!(self.num_cold + self.num_hot, 0);
        debug_assert_ne!(self.weight_cold + self.weight_hot, 0);
        loop {
            let idx = if let Some(idx) = self.cold_head {
                idx
            } else {
                self.advance_hot(lcs);
                return;
            };
            let (entry, next) = self.entries.get_mut(idx).unwrap();
            let Entry::Resident(resident) = entry else {
                unreachable!()
            };
            debug_assert_eq!(resident.state, ResidentState::Cold);
            if *resident.referenced.get_mut() != 0 {
                resident.state = ResidentState::Hot;
                let weight = self.weighter.weight(&resident.key, &resident.value);
                self.weight_hot += weight;
                self.weight_cold -= weight;
                self.num_hot += 1;
                self.num_cold -= 1;
                Self::relink(
                    &mut self.entries,
                    idx,
                    &mut self.cold_head,
                    &mut self.hot_head,
                );
                // evict from hot if overweight
                while self.weight_hot > self.weight_target_hot {
                    self.advance_hot(lcs);
                }
                return;
            }

            let weight = self.weighter.weight(&resident.key, &resident.value);
            if weight == 0 {
                if self.weight_cold == 0 {
                    self.advance_hot(lcs);
                    return;
                } else {
                    self.cold_head = Some(next);
                    continue;
                }
            }
            self.weight_cold -= weight;
            self.lifecycle
                .before_evict(lcs, &resident.key, &mut resident.value);
            if self.weighter.weight(&resident.key, &resident.value) == 0 {
                self.cold_head = Some(next);
                return;
            }
            let hash = Self::hash_static(&self.hash_builder, &resident.key);
            let Entry::Resident(evicted) = mem::replace(entry, Entry::Ghost(hash)) else {
                unreachable!()
            };
            Self::relink(
                &mut self.entries,
                idx,
                &mut self.cold_head,
                &mut self.ghost_head,
            );
            self.num_cold -= 1;
            self.num_non_resident += 1;
            // evict from ghost if oversized
            if self.num_non_resident > self.capacity_non_resident {
                self.advance_ghost();
            }
            self.lifecycle.on_evict(lcs, evicted.key, evicted.value);
            return;
        }
    }

    /// Advance hot ring evicting entries.
    /// Panics if there are no hot entries.
    #[inline]
    fn advance_hot(&mut self, lcs: &mut L::RequestState) {
        debug_assert_ne!(self.num_hot, 0);
        debug_assert_ne!(self.weight_hot, 0);
        loop {
            let idx = self.hot_head.unwrap();
            let (entry, next) = self.entries.get_mut(idx).unwrap();
            let Entry::Resident(resident) = entry else {
                unreachable!()
            };
            debug_assert_eq!(resident.state, ResidentState::Hot);
            if *resident.referenced.get_mut() != 0 {
                *resident.referenced.get_mut() = (*resident.referenced.get_mut()).min(MAX_F) - 1;
                self.hot_head = Some(next);
                continue;
            }
            let weight = self.weighter.weight(&resident.key, &resident.value);
            if weight == 0 {
                self.hot_head = Some(next);
                continue;
            }
            self.weight_hot -= weight;
            self.lifecycle
                .before_evict(lcs, &resident.key, &mut resident.value);
            if self.weighter.weight(&resident.key, &resident.value) == 0 {
                self.hot_head = Some(next);
            } else {
                self.num_hot -= 1;
                let hash = Self::hash_static(&self.hash_builder, &resident.key);
                let (entry, next) = self.entries.remove(idx).unwrap();
                self.hot_head = next;
                self.map_remove(hash, idx);
                let Entry::Resident(evicted) = entry else {
                    unreachable!()
                };
                self.lifecycle.on_evict(lcs, evicted.key, evicted.value);
            }
            return;
        }
    }

    #[inline]
    fn advance_ghost(&mut self) {
        debug_assert_ne!(self.num_non_resident, 0);
        let idx = self.ghost_head.unwrap();
        let (entry, _) = self.entries.get_mut(idx).unwrap();
        let Entry::Ghost(hash) = *entry else {
            unreachable!()
        };
        self.num_non_resident -= 1;
        self.map_remove(hash, idx);
        let (_, next) = self.entries.remove(idx).unwrap();
        self.ghost_head = next;
    }

    fn insert_existing(
        &mut self,
        lcs: &mut L::RequestState,
        idx: Token,
        key: Key,
        value: Val,
        weight: u64,
        strategy: InsertStrategy,
    ) -> Result<(), (Key, Val)> {
        // caller must have already handled overweight items
        debug_assert!(weight <= self.weight_target_hot);
        let (entry, _) = self.entries.get_mut(idx).unwrap();
        let referenced;
        let enter_state;
        match entry {
            Entry::Resident(resident) => {
                enter_state = resident.state;
                referenced = *resident.referenced.get_mut() != 0
                    || matches!(strategy, InsertStrategy::Replace { soft: false });
            }
            _ if matches!(strategy, InsertStrategy::Replace { .. }) => {
                return Err((key, value));
            }
            Entry::Ghost(_) => {
                referenced = false;
                enter_state = ResidentState::Hot;
            }
            Entry::Placeholder(ph) => {
                referenced = true;
                enter_state = ph.hot;
            }
        }

        let evicted = mem::replace(
            entry,
            Entry::Resident(Resident {
                key,
                value,
                state: enter_state,
                referenced: (referenced as u16).into(),
            }),
        );
        match evicted {
            Entry::Resident(evicted) => {
                debug_assert_eq!(evicted.state, enter_state);
                let evicted_weight = self.weighter.weight(&evicted.key, &evicted.value);
                if enter_state == ResidentState::Hot {
                    self.weight_hot -= evicted_weight;
                    self.weight_hot += weight;
                } else {
                    self.weight_cold -= evicted_weight;
                    self.weight_cold += weight;
                }
                self.lifecycle.on_evict(lcs, evicted.key, evicted.value);
            }
            Entry::Ghost(_) => {
                self.weight_hot += weight;
                self.num_hot += 1;
                self.num_non_resident -= 1;
                Self::relink(
                    &mut self.entries,
                    idx,
                    &mut self.ghost_head,
                    &mut self.hot_head,
                );
            }
            Entry::Placeholder(_) => {
                let list_head = if enter_state == ResidentState::Hot {
                    self.num_hot += 1;
                    self.weight_hot += weight;
                    &mut self.hot_head
                } else {
                    self.num_cold += 1;
                    self.weight_cold += weight;
                    &mut self.cold_head
                };
                self.entries.link(idx, *list_head);
                list_head.get_or_insert(idx);
            }
        }

        while self.weight_hot + self.weight_cold > self.weight_capacity {
            self.advance_cold(lcs);
        }
        Ok(())
    }

    #[inline]
    fn relink<T>(
        ring: &mut LinkedSlab<T>,
        idx: Token,
        source_head: &mut Option<Token>,
        target_head: &mut Option<Token>,
    ) {
        let next = ring.unlink(idx);
        if *source_head == Some(idx) {
            *source_head = next;
        }
        ring.link(idx, *target_head);
        if target_head.is_none() {
            *target_head = Some(idx);
        }
    }

    #[inline]
    fn map_insert(&mut self, hash: u64, idx: Token) {
        self.map.insert(hash, idx, |&i| {
            let (entry, _) = self.entries.get(i).unwrap();
            match entry {
                Entry::Resident(Resident { key, .. })
                | Entry::Placeholder(Placeholder { key, .. }) => {
                    Self::hash_static(&self.hash_builder, key)
                }
                Entry::Ghost(hash) => *hash,
            }
        });
    }

    #[inline]
    fn map_remove(&mut self, hash: u64, idx: Token) {
        let removed = self.map.erase_entry(hash, |&i| i == idx);
        debug_assert!(removed);
    }

    pub fn replace_placeholder(
        &mut self,
        lcs: &mut L::RequestState,
        placeholder: &Plh,
        referenced: bool,
        mut value: Val,
    ) -> Result<(), Val> {
        let entry = match self.entries.get_mut(placeholder.idx()) {
            Some((entry, _)) if matches!(&*entry, Entry::Placeholder(p) if p.shared.same_as(placeholder)) => {
                entry
            }
            _ => return Err(value),
        };
        let Entry::Placeholder(Placeholder {
            key,
            hot: mut placeholder_hot,
            ..
        }) = mem::replace(entry, Entry::Ghost(0))
        else {
            unreachable!()
        };
        let mut weight = self.weighter.weight(&key, &value);
        // don't admit if it won't fit within the budget
        if weight > self.weight_target_hot {
            self.lifecycle.before_evict(lcs, &key, &mut value);
            weight = self.weighter.weight(&key, &value);
            if weight > self.weight_target_hot {
                return self.handle_overweight_replace_placeholder(lcs, placeholder, key, value);
            }
        }

        if self.weight_hot + self.weight_cold + weight <= self.weight_capacity {
            placeholder_hot = ResidentState::Hot;
        }
        *entry = Entry::Resident(Resident {
            key,
            value,
            state: placeholder_hot,
            referenced: (referenced as u16).into(),
        });

        let list_head = if placeholder_hot == ResidentState::Hot {
            self.num_hot += 1;
            self.weight_hot += weight;
            &mut self.hot_head
        } else {
            self.num_cold += 1;
            self.weight_cold += weight;
            &mut self.cold_head
        };
        self.entries.link(placeholder.idx(), *list_head);
        list_head.get_or_insert(placeholder.idx());

        while self.weight_hot + self.weight_cold > self.weight_capacity {
            self.advance_cold(lcs);
        }

        Ok(())
    }

    #[cold]
    fn handle_overweight_replace_placeholder(
        &mut self,
        lcs: &mut L::RequestState,
        placeholder: &Plh,
        key: Key,
        value: Val,
    ) -> Result<(), Val> {
        self.entries.remove(placeholder.idx());
        self.map_remove(placeholder.hash(), placeholder.idx());
        self.lifecycle.on_evict(lcs, key, value);
        Ok(())
    }

    pub fn insert(
        &mut self,
        lcs: &mut L::RequestState,
        hash: u64,
        key: Key,
        mut value: Val,
        strategy: InsertStrategy,
    ) -> Result<(), (Key, Val)> {
        let mut weight = self.weighter.weight(&key, &value);
        // don't admit if it won't fit within the budget
        if weight > self.weight_target_hot {
            self.lifecycle.before_evict(lcs, &key, &mut value);
            weight = self.weighter.weight(&key, &value);
            if weight > self.weight_target_hot {
                return self.handle_insert_overweight(lcs, hash, key, value, strategy);
            }
        }

        if let Some(idx) = self.search(hash, &key) {
            return self.insert_existing(lcs, idx, key, value, weight, strategy);
        } else if matches!(strategy, InsertStrategy::Replace { .. }) {
            return Err((key, value));
        }

        let enter_hot = if self.weight_hot + self.weight_cold + weight > self.weight_capacity {
            // evict until we have enough space for this entry
            loop {
                self.advance_cold(lcs);
                if self.weight_hot + self.weight_cold + weight <= self.weight_capacity {
                    break;
                }
            }
            false
        } else {
            // cache if filling, admit as hot if possible
            self.weight_hot + weight <= self.weight_target_hot
        };

        let (state, list_head) = if enter_hot {
            self.num_hot += 1;
            self.weight_hot += weight;
            (ResidentState::Hot, &mut self.hot_head)
        } else {
            self.num_cold += 1;
            self.weight_cold += weight;
            (ResidentState::Cold, &mut self.cold_head)
        };
        let idx = self.entries.insert(
            Entry::Resident(Resident {
                key,
                value,
                state,
                referenced: Default::default(),
            }),
            *list_head,
        );
        list_head.get_or_insert(idx);
        self.map_insert(hash, idx);
        Ok(())
    }

    #[cold]
    fn handle_insert_overweight(
        &mut self,
        lcs: &mut L::RequestState,
        hash: u64,
        key: Key,
        value: Val,
        strategy: InsertStrategy,
    ) -> Result<(), (Key, Val)> {
        // Make sure to remove any existing entry
        if let Some((idx, _)) = self.search_resident(hash, &key) {
            if let Some((ek, ev)) = self.remove_internal(hash, idx) {
                self.lifecycle.on_evict(lcs, ek, ev);
            }
        }
        if matches!(strategy, InsertStrategy::Replace { .. }) {
            return Err((key, value));
        }
        self.lifecycle.on_evict(lcs, key, value);
        Ok(())
    }

    pub fn upsert_placeholder<Q>(
        &mut self,
        hash: u64,
        key: &Q,
    ) -> Result<(Token, &Val), (Plh, bool)>
    where
        Q: Hash + Equivalent<Key> + ToOwned<Owned = Key> + ?Sized,
    {
        let shared;
        if let Some(idx) = self.search(hash, key) {
            let (entry, _) = self.entries.get_mut(idx).unwrap();
            match entry {
                Entry::Resident(resident) => {
                    if *resident.referenced.get_mut() < MAX_F {
                        *resident.referenced.get_mut() += 1;
                    }
                    record_hit_mut!(self);
                    unsafe {
                        // Rustc gets insanely confused returning references from mut borrows
                        // Safety: value will have the same lifetime as `resident`
                        let value_ptr: *const Val = &resident.value;
                        return Ok((idx, &*value_ptr));
                    }
                }
                Entry::Placeholder(p) => {
                    record_hit_mut!(self);
                    return Err((p.shared.clone(), false));
                }
                Entry::Ghost(_) => {
                    shared = Plh::new(hash, idx);
                    *entry = Entry::Placeholder(Placeholder {
                        key: key.to_owned(),
                        hot: ResidentState::Hot,
                        shared: shared.clone(),
                    });
                    self.num_non_resident -= 1;
                    let next = self.entries.unlink(idx);
                    if self.ghost_head == Some(idx) {
                        self.ghost_head = next;
                    }
                }
            }
        } else {
            let idx = self.entries.next_free();
            shared = Plh::new(hash, idx);
            let idx_ = self.entries.insert(
                Entry::Placeholder(Placeholder {
                    key: key.to_owned(),
                    hot: ResidentState::Cold,
                    shared: shared.clone(),
                }),
                None,
            );
            debug_assert_eq!(idx, idx_);
            self.map_insert(hash, idx);
        }
        record_miss_mut!(self);
        Err((shared, true))
    }
}

/// Structure wrapping a mutable reference to a cached item.
pub struct RefMut<'cache, Key, Val, We: Weighter<Key, Val>> {
    key: &'cache Key,
    value: &'cache mut Val,
    weight: &'cache mut u64,
    weighter: &'cache We,
}

impl<'cache, Key, Val, We: Weighter<Key, Val>> std::ops::Deref for RefMut<'cache, Key, Val, We> {
    type Target = Val;

    fn deref(&self) -> &Self::Target {
        self.value
    }
}

impl<'cache, Key, Val, We: Weighter<Key, Val>> std::ops::DerefMut for RefMut<'cache, Key, Val, We> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value
    }
}

impl<'cache, Key, Val, We: Weighter<Key, Val>> Drop for RefMut<'cache, Key, Val, We> {
    fn drop(&mut self) {
        *self.weight += self.weighter.weight(self.key, self.value);
    }
}
