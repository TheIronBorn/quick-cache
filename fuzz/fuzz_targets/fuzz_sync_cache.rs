#![no_main]
use std::time::Duration;

use ahash::{HashMap, HashSet};
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use quick_cache::{
    sync::{Cache, GuardResult},
    Lifecycle, OptionsBuilder, Weighter,
};

#[derive(Clone)]
struct MyWeighter;

#[derive(Clone)]
struct MyLifecycle;

#[derive(Debug, Clone, Copy)]
struct Value {
    original: u16,
    current: u16,
    pinned: bool,
}

impl Weighter<u16, Value> for MyWeighter {
    fn weight(&self, _key: &u16, val: &Value) -> u64 {
        val.current as u64
    }
}

impl Lifecycle<u16, Value> for MyLifecycle {
    type RequestState = Vec<(u16, Value)>;

    fn begin_request(&self) -> Self::RequestState {
        Default::default()
    }

    fn is_pinned(&self, _key: &u16, val: &Value) -> bool {
        val.pinned
    }

    fn before_evict(&self, _state: &mut Self::RequestState, _key: &u16, val: &mut Value) {
        // eprintln!("Before evict {_key} {val:?}");
        if val.original % 5 == 0 {
            val.current = 0;
        }
    }

    fn on_evict(&self, state: &mut Self::RequestState, key: u16, val: Value) {
        // eprintln!("Evicted {key}");
        state.push((key, val));
    }
}

#[derive(Debug, Arbitrary)]
enum Op {
    Insert(u16, u16, bool),
    Replace(u16, u16, bool),
    Placeholder(u16),
    SetPlaceholder(u16, u16, bool),
    Remove(u16),
}

#[derive(Debug, Arbitrary)]
struct Input {
    seed: u32,
    estimated_items_capacity: u16,
    weight_capacity: u32,
    hot_allocation: u8,
    ghost_allocation: u8,
    operations: Vec<Op>,
}

fuzz_target!(|input: Input| {
    run(input);
});

fn run(input: Input) {
    let Input {
        seed,
        operations,
        estimated_items_capacity,
        weight_capacity,
        hot_allocation,
        ghost_allocation,
    } = input;
    let hasher = ahash::RandomState::with_seed(seed as usize);
    let estimated_items_capacity = estimated_items_capacity as usize;
    let weight_capacity = weight_capacity as u64;
    let hot_allocation = hot_allocation as f64 / (u8::MAX as f64);
    let ghost_allocation = ghost_allocation as f64 / (u8::MAX as f64);
    let options = OptionsBuilder::new()
        .estimated_items_capacity(estimated_items_capacity)
        .weight_capacity(weight_capacity)
        .hot_allocation(hot_allocation)
        .ghost_allocation(ghost_allocation)
        .shards(1)
        .build()
        .unwrap();
    let cache = Cache::with_options(options, MyWeighter, hasher, MyLifecycle);
    let mut placeholders: HashMap<u16, _> = HashMap::default();
    for op in operations {
        match op {
            Op::Insert(k, v, pinned) => {
                // eprintln!("insert {k} {v}");
                let evicted = cache.insert_with_lifecycle(
                    k,
                    Value {
                        original: v,
                        current: v,
                        pinned,
                    },
                );
                placeholders.remove(&k);
                // if k is present it must have value v
                let peek = cache.peek(&k);
                assert!(peek.is_none() || peek.unwrap().original == v);
                check_evicted(k, peek, evicted);
            }
            Op::Replace(k, v, pinned) => {
                // eprintln!("replace {k} {v}");
                placeholders.remove(&k);
                if let Ok(evicted) = cache.replace_with_lifecycle(
                    k,
                    Value {
                        original: v,
                        current: v,
                        pinned,
                    },
                    false,
                ) {
                    // if k is present it must have value v
                    let peek = cache.peek(&k);
                    assert!(peek.is_none() || peek.unwrap().original == v);
                    check_evicted(k, peek, evicted);
                } else {
                    assert!(cache.peek(&k).is_none());
                }
            }
            Op::Placeholder(k) => {
                // eprintln!("get_value_or_guard {k}");
                match cache.get_value_or_guard(&k, Some(Duration::default())) {
                    GuardResult::Value(_gv) => {}
                    GuardResult::Guard(g) => {
                        placeholders.insert(k, g);
                    }
                    GuardResult::Timeout => assert!(placeholders.contains_key(&k)),
                }
            }
            Op::SetPlaceholder(k, v, pinned) => {
                if let Some(p) = placeholders.remove(&k) {
                    let value = Value {
                        original: v,
                        current: v,
                        pinned,
                    };
                    if let Ok(evicted) = p.insert_with_lifecycle(value) {
                        let peek = cache.peek(&k);
                        assert!(peek.is_none() || peek.unwrap().original == v);
                        check_evicted(k, peek, evicted);
                    }
                }
            }
            Op::Remove(k) => {
                // eprintln!("remove {k}");
                if let Some((rem_k, _)) = cache.remove(&k) {
                    placeholders.remove(&k);
                    assert_eq!(rem_k, k);
                }
                assert!(cache.peek(&k).is_none());
            }
        }
    }
    cache.validate();
}

fn check_evicted(key: u16, get: Option<Value>, evicted: Vec<(u16, Value)>) {
    let mut evicted_hm = HashSet::default();
    evicted_hm.reserve(evicted.len());
    for (ek, ev) in evicted {
        // we can't evict a 0 weight item, unless it was replaced
        assert!(ev.current != 0 || ek == key);
        // we can't evict a pinned item, unless it was replaced
        assert!(!ev.pinned || ek == key);
        // we can't evict something twice, except if the insert displaced an old value but the new value also got evicted
        assert!(evicted_hm.insert(ek) || (ek == key && get.is_none()));
    }
}
