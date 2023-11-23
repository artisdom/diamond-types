//! This file contains tools to manage the document as a time dag. Specifically, tools to tell us
//! about branches, find diffs and move between branches.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use smallvec::{smallvec, SmallVec};
use rle::{AppendRle, SplitableSpan};

use crate::frontier::{debug_assert_sorted, FrontierRef};
use crate::causalgraph::graph::Graph;
use crate::causalgraph::graph::tools::DiffFlag::*;
use crate::dtrange::DTRange;
use crate::{Frontier, LV};

#[cfg(feature = "serde")]
use serde::Serialize;

// Diff function needs to tag each entry in the queue based on whether its part of a's history or
// b's history or both, and do so without changing the sort order for the heap.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub(crate) enum DiffFlag { OnlyA, OnlyB, Shared }

impl Graph {
    fn shadow_of(&self, time: LV) -> LV {
        self.entries.find(time).unwrap().shadow
    }

    /// Does the frontier `[a]` contain `[b]` as a direct ancestor according to its shadow?
    fn txn_shadow_contains(&self, a: LV, b: LV) -> bool {

        // wrapping_add(1) so we compute ROOT correctly.
        a == b || (a > b && self.shadow_of(a) <= b)
        // let a_1 = a.wrapping_add(1);
        // let b_1 = b.wrapping_add(1);
        // a_1 == b_1 || (a_1 > b_1 && self.shadow_of(a).wrapping_add(1) <= b_1)
    }

    /// This is similar to txn_shadow_contains, but it also checks that a doesn't have any other
    /// ancestors which aren't included in b's history. Eg:
    ///
    /// ```text
    /// 1
    /// | 2
    /// \ /
    ///  3
    /// ```
    ///
    /// `txn_shadow_contains(3, 2)` is true, but `is_direct_descendant_coarse(3, 2)` is false.
    ///
    /// See `diff_shadow_bubble` test below for an example.
    pub(crate) fn is_direct_descendant_coarse(&self, a: LV, b: LV) -> bool {
        // This is a bit more strict than we technically need, but its fast for short circuit
        // evaluation.
        a == b || (a > b && self.entries.find(a).unwrap().contains(b))
        // a == b
        //     || (b == ROOT_TIME && self.txn_shadow_contains(a, ROOT_TIME))
        //     || (a != ROOT_TIME && a > b && self.0.find(a).unwrap().contains(b))
    }

    /// Compare two versions and figure out how they relate.
    ///
    /// * If the operations are concurrent, this returns `None`.
    /// * If they're numerically equal it returns `Some(Equal)`.
    /// * Otherwise it returns `Some(Greater)` or `Some(Lesser)` depending on which operation
    ///   dominates the other.
    pub fn version_cmp(&self, v1: LV, v2: LV) -> Option<Ordering> {
        match v1.cmp(&v2) {
            Ordering::Equal => Some(Ordering::Equal),
            Ordering::Less => {
                if self.frontier_contains_version(&[v2], v1) {
                    Some(Ordering::Less)
                } else {
                    None
                }
            },
            Ordering::Greater => {
                if self.frontier_contains_version(&[v1], v2) {
                    Some(Ordering::Greater)
                } else {
                    None
                }
            },
        }
    }

    /// Calculates whether the specified version contains (dominates) the specified time.
    pub(crate) fn frontier_contains_version(&self, frontier: &[LV], target: LV) -> bool {
        if frontier.contains(&target) { return true; }
        if frontier.is_empty() { return false; }

        // Fast path. This causes extra calls to find_packed(), but you usually have a branch with
        // a shadow less than target. Usually the root document. And in that case this codepath
        // avoids the allocation from BinaryHeap.
        for &o in frontier {
            if o > target {
                let txn = self.entries.find(o).unwrap();
                if txn.shadow_contains(target) { return true; }
            }
        }

        // So I don't *need* to use a priority queue here. The options are:
        // 1. Use a priority queue, scanning from the highest to lowest orders
        // 2. Use a simple list and do DFS, potentially scanning some items twice
        // 3. Use a simple list and do DFS, with another structure to mark which items we've
        //    visited.
        //
        // Honestly any approach should be obnoxiously fast in any real editing session anyway.

        // TODO: Consider moving queue into a threadlocal variable so we don't need to reallocate it
        // with each call.
        let mut queue = BinaryHeap::new();

        // This code could be written to use parent_indexes but its a bit tricky, as an index isn't
        // enough specificity. We'd need the parent and the parent_index. Eh...
        for &o in frontier {
            debug_assert_ne!(o, target);
            if o > target { queue.push(o); }
        }

        while let Some(order) = queue.pop() {
            debug_assert!(order > target);
            // dbg!((order, &queue));

            // TODO: Skip these calls to find() using parent_index.
            let entry = self.entries.find_packed(order);
            if entry.shadow_contains(target) { return true; }

            while let Some(&next_time) = queue.peek() {
                if next_time >= entry.span.start {
                    // dbg!(next_order);
                    queue.pop();
                } else { break; }
            }

            // dbg!(order);
            for &p in entry.parents.iter() {
                #[allow(clippy::comparison_chain)]
                if p == target { return true; }
                else if p > target { queue.push(p); }
                // If p < target, it can't be a child of target. So we can discard it.
            }
        }

        false
    }

    /// Does frontier *a* contain (dominate) frontier *b*? Note, if this method returns false, there
    /// are a few different cases. This is not reflexive.
    pub fn frontier_contains_frontier(&self, a: &[LV], b: &[LV]) -> bool {
        if a == b { return true; } // Might be a pointless optimization.

        for bb in b {
            if !self.frontier_contains_version(a, *bb) { return false; }
        }
        true
    }
}

pub(crate) type DiffResult = (SmallVec<[DTRange; 4]>, SmallVec<[DTRange; 4]>);

impl Graph {
    /// Returns (spans only in a, spans only in b). Spans are in natural (ascending) order.
    ///
    /// Also find which operation is the greatest common ancestor.
    pub fn diff(&self, a: &[LV], b: &[LV]) -> DiffResult {
        let mut result = self.diff_rev(a, b);
        result.0.reverse();
        result.1.reverse();
        result
    }

    /// Returns (spans only in a, spans only in b). Spans are in reverse (descending) order.
    ///
    /// Also find which operation is the greatest common ancestor.
    pub fn diff_rev(&self, a: &[LV], b: &[LV]) -> DiffResult {
        // First some simple short circuit checks to avoid needless work in common cases.
        // Note most of the time this method is called, one of these early short circuit cases will
        // fire.
        if a == b { return (smallvec![], smallvec![]); }

        if a.len() == 1 && b.len() == 1 {
            // Check if either operation naively dominates the other. We could do this for more
            // cases, but we may as well use the code below instead.
            let a = a[0];
            let b = b[0];
            if a == b { return (smallvec![], smallvec![]); }

            if self.is_direct_descendant_coarse(a, b) {
                // a >= b.
                return (smallvec![(b.wrapping_add(1)..a.wrapping_add(1)).into()], smallvec![]);
                // return (smallvec![(b.wrapping_add(1)..a.wrapping_add(1)).into()], smallvec![], b);
            }
            if self.is_direct_descendant_coarse(b, a) {
                // b >= a.
                return (smallvec![], smallvec![(a.wrapping_add(1)..b.wrapping_add(1)).into()]);
                // return (smallvec![], smallvec![(a.wrapping_add(1)..b.wrapping_add(1)).into()], a);
            }
        }

        // Otherwise fall through to the slow version.
        self.diff_slow(a, b)
    }

    fn diff_slow(&self, a: &[LV], b: &[LV]) -> DiffResult {
        let mut only_a = smallvec![];
        let mut only_b = smallvec![];

        // marks range [ord_start..ord_end] *inclusive* with flag in our output.
        let mark_run = |ord_start, ord_end, flag: DiffFlag| {
            let target = match flag {
                OnlyA => { &mut only_a }
                OnlyB => { &mut only_b }
                Shared => { return; }
            };
            // dbg!((ord_start, ord_end));

            target.push_reversed_rle(DTRange::new(ord_start, ord_end + 1));
        };

        self.diff_slow_internal(a, b, mark_run);
        (only_a, only_b)
    }

    fn diff_slow_internal<F>(&self, a: &[LV], b: &[LV], mut mark_run: F)
        where F: FnMut(LV, LV, DiffFlag) {
        // Sorted highest to lowest.
        let mut queue: BinaryHeap<(LV, DiffFlag)> = BinaryHeap::new();
        for a_ord in a {
            queue.push((*a_ord, OnlyA));
        }
        for b_ord in b {
            queue.push((*b_ord, OnlyB));
        }

        let mut num_shared_entries = 0;

        while let Some((mut ord, mut flag)) = queue.pop() {
            if flag == Shared { num_shared_entries -= 1; }

            // dbg!((ord, flag));
            while let Some((peek_ord, peek_flag)) = queue.peek() {
                if *peek_ord != ord { break; } // Normal case.
                else {
                    // 3 cases if peek_flag != flag. We set flag = Shared in all cases.
                    if *peek_flag != flag { flag = Shared; }
                    if *peek_flag == Shared { num_shared_entries -= 1; }
                    queue.pop();
                }
            }

            // Grab the txn containing ord. This will usually be at prev_txn_idx - 1.
            // TODO: Remove usually redundant binary search

            let containing_txn = self.entries.find_packed(ord);

            // There's essentially 2 cases here:
            // 1. This item and the first item in the queue are part of the same txn. Mark down to
            //    the queue head and continue.
            // 2. Its not. Mark the whole txn and queue parents.

            // 1:
            while let Some((peek_ord, peek_flag)) = queue.peek() {
                // dbg!((peek_ord, peek_flag));
                if *peek_ord < containing_txn.span.start { break; } else {
                    if *peek_flag != flag {
                        // Mark from peek_ord..=ord and continue.
                        // Note we'll mark this whole txn from ord, but we might do so with
                        // different flags.
                        mark_run(*peek_ord + 1, ord, flag);
                        ord = *peek_ord;
                        // offset -= ord - peek_ord;
                        flag = Shared;
                    }
                    if *peek_flag == Shared { num_shared_entries -= 1; }
                    queue.pop();
                }
            }

            // 2: Mark the rest of the txn in our current color and repeat. Note we still need to
            // mark the run even if ord == containing_txn.order because the spans are inclusive.
            mark_run(containing_txn.span.start, ord, flag);

            for p in containing_txn.parents.iter() {
                queue.push((*p, flag));
                if flag == Shared { num_shared_entries += 1; }
            }

            // If there's only shared entries left, abort.
            if queue.len() == num_shared_entries { break; }
        }
    }

    // *** Conflicts! ***

    fn find_conflicting_slow<V>(&self, a: &[LV], b: &[LV], mut visit: V) -> Frontier
    where V: FnMut(DTRange, DiffFlag) {
        // dbg!(a, b);

        // Sorted highest to lowest (so we get the highest item first).
        #[derive(Debug, PartialEq, Eq, Clone)]
        struct TimePoint {
            // For merges this is the highest time. usize::MAX for ROOT time. Ord implementation
            // below makes sure ROOT gets sorted last.
            last: LV,
            // TODO: Compare performance here with actually using a vec.
            merged_with: SmallVec<[LV; 1]>, // Always sorted. Usually empty.
        }

        impl Ord for TimePoint {
            #[inline(always)]
            fn cmp(&self, other: &Self) -> Ordering {
                // wrapping_add(1) converts ROOT into 0 for proper comparisons.
                // TODO: Consider pulling this out
                self.last.wrapping_add(1).cmp(&other.last.wrapping_add(1))
                    .then_with(|| other.merged_with.len().cmp(&self.merged_with.len()))
            }
        }

        impl PartialOrd for TimePoint {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                Some(self.cmp(other))
            }
        }

        impl From<LV> for TimePoint {
            fn from(time: LV) -> Self {
                Self { last: time, merged_with: smallvec![] }
            }
        }

        impl From<&[LV]> for TimePoint {
            fn from(version: FrontierRef) -> Self {
                // debug_assert!(frontier_is_sorted(version));

                Self {
                    // Bleh.
                    last: *version.last().unwrap_or(&usize::MAX),
                    merged_with: if version.len() > 1 {
                        SmallVec::from_slice(&version[..version.len() - 1])
                    } else {
                        smallvec![]
                    }
                }
            }
        }

        // The heap is sorted such that we pull the highest items first.
        let mut queue: BinaryHeap<(TimePoint, DiffFlag)> = BinaryHeap::new();
        queue.push((a.into(), OnlyA));
        queue.push((b.into(), OnlyB));

        // Loop until we've collapsed the graph down to a single element.
        let frontier: Frontier = 'outer: loop {
            let (time, mut flag) = queue.pop().unwrap();
            let t = time.last;
            // dbg!((&time, flag));

            if t == usize::MAX { break Frontier::root(); }

            // Discard duplicate entries.

            // I could write this with an inner loop and a match statement, but this is shorter and
            // more readable. The optimizer has to earn its keep somehow.
            while let Some((peek_time, peek_flag)) = queue.peek() {
                if *peek_time == time { // NOTE: This compares the whole frontier!
                    // Logic adapted from diff().
                    if *peek_flag != flag { flag = Shared; }
                    queue.pop();
                } else { break; }
            }

            if queue.is_empty() {
                // In this order because time.last > time.merged_with.
                let mut frontier: Frontier = Frontier::from_sorted(time.merged_with.as_slice());
                // branch.extend(time.merged_with.into_iter());
                frontier.0.push(t);
                frontier.debug_check_sorted();
                debug_assert_eq!(flag, Shared);
                break frontier;
            }

            // If this node is a merger, shatter it.
            if !time.merged_with.is_empty() {
                // We'll deal with time.last directly this loop iteration.
                for t in time.merged_with {
                    queue.push((t.into(), flag));
                }
            }

            let containing_txn = self.entries.find_packed(t);

            // I want an inclusive iterator :p
            let mut range = DTRange { start: containing_txn.span.start, end: t + 1 };

            // Consume all other changes within this txn.
            loop {
                if let Some((peek_time, _peek_flag)) = queue.peek() {
                    // println!("peek {:?}", &peek_time);
                    // Might be simpler to use containing_txn.contains(peek_time.last).
                    if peek_time.last != usize::MAX && peek_time.last >= containing_txn.span.start {
                        // The next item is within this txn. Consume it.
                        // dbg!((&peek_time, peek_flag));
                        let (time, next_flag) = queue.pop().unwrap();

                        // Only emit inner items when they aren't duplicates.
                        if time.last + 1 < range.end {
                            // +1 because we don't want to include the actual merge point in the returned set.
                            let offset = time.last + 1 - containing_txn.span.start;
                            debug_assert!(offset > 0);
                            let rem = range.truncate(offset);

                            visit(rem, flag);
                        }
                        // result.push_reversed_rle(rem);

                        if !time.merged_with.is_empty() {
                            // We've run into a merged item which uses part of this entry.
                            // We've already pushed the necessary span to the result. Do the
                            // normal merge & shatter logic with this item next.
                            for t in time.merged_with {
                                queue.push((t.into(), next_flag));
                            }
                        }

                        if next_flag != flag { flag = Shared; }
                    } else {
                        // Emit the remainder of this txn.
                        visit(range, flag);
                        // result.push_reversed_rle(range);

                        // If this entry has multiple parents, we'll push a merge here then
                        // immediately pop it. This is so we stop at the merge point.
                        queue.push((containing_txn.parents.as_ref().into(), flag));
                        break;
                    }
                } else {
                    // println!("XXXX {:?}", &range.last());
                    break 'outer Frontier::new_1(range.last());
                }
            }
        };

        frontier
    }

    /// This method is used to find the operation ranges we need to look at that might be concurrent
    /// with incoming edits.
    ///
    /// We need to track all spans back to a *single point in time*. This point in time is usually
    /// a single localtime, but it might be the result of a merge of multiple edits.
    ///
    /// I'm assuming b is a parent of a, but it should all work if thats not the case.
    pub(crate) fn find_conflicting<V>(&self, a: &[LV], b: &[LV], mut visit: V) -> Frontier
        where V: FnMut(DTRange, DiffFlag) {

        // First some simple short circuit checks to avoid needless work in common cases.
        // Note most of the time this method is called, one of these early short circuit cases will
        // fire.
        if a == b {
            return a.into();
        }

        if a.len() == 1 && b.len() == 1 {
            // Check if either operation naively dominates the other. We could do this for more
            // cases, but we may as well use the code below instead.
            let a = a[0];
            let b = b[0];

            if self.is_direct_descendant_coarse(a, b) {
                // a >= b.
                visit((b.wrapping_add(1)..a.wrapping_add(1)).into(), OnlyA);
                return Frontier::new_1(b);
            }
            if self.is_direct_descendant_coarse(b, a) {
                // b >= a.
                visit((a.wrapping_add(1)..b.wrapping_add(1)).into(), OnlyB);
                return Frontier::new_1(a);
            }
        }

        // Otherwise fall through to the slow version.
        self.find_conflicting_slow(a, b, visit)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ConflictZone {
    pub(crate) common_ancestor: Frontier,
    pub(crate) rev_spans: SmallVec<[DTRange; 4]>,
}

impl Graph {
    // Turns out I'm not finding this variant useful. Might be worth discarding it?
    #[allow(unused)]
    pub(crate) fn find_conflicting_simple(&self, a: &[LV], b: &[LV]) -> ConflictZone {
        let mut rev_spans = smallvec![];
        let common_ancestor = self.find_conflicting(a, b, |span, _flag| {
            rev_spans.push_reversed_rle(span);
        });

        ConflictZone { common_ancestor, rev_spans }
    }

    /// This is a variant of find_dominators_full for larger sets of versions - eg for all the
    /// versions in the history of a single item.
    ///
    /// Even with very big lists, this should be pretty quick. However, unlike find_dominators_full:
    ///
    /// - This doesn't yield the non-dominator items in the set.
    /// - This method requires the input versions to be fully sorted.
    pub fn find_dominators_wide_rev(&self, versions: &[LV]) -> SmallVec<[LV; 2]> {
        if versions.len() <= 1 { return versions.into(); }

        let mut min_v = versions[0];
        let mut max_v = versions[0];
        for &v in &versions[1..] {
            min_v = min_v.min(v);
            max_v = max_v.max(v);
        }

        let last_entry = self.entries.find_packed(max_v);
        // Nothing else in the list matters because its all under the shadow of this item.
        // This is the most common case.
        if last_entry.shadow <= min_v { return smallvec![max_v]; }

        let mut result_rev = smallvec![];

        self.find_dominators_full_internal(versions.iter().copied(), min_v, |v, dom| {
            if dom {
                result_rev.push(v);
            }
        });

        result_rev
    }

    pub fn find_dominators(&self, versions: &[LV]) -> Frontier {
        let mut result = self.find_dominators_wide_rev(versions);
        result.reverse();
        Frontier(result)
    }

    /// This method assumes v_1 and v_2 are already dominators.
    pub fn find_dominators_2(&self, v_1: &[LV], v_2: &[LV]) -> Frontier {
        if v_1.is_empty() { return v_2.into(); }
        if v_2.is_empty() { return v_1.into(); }

        if v_1.len() == 1 && v_2.len() == 1 {
            // There's 4 cases: v_1 == v_2, v_1 > v_2, v_1 < v_2 or v_1 || v_2.
            let a = v_1[0];
            let b = v_2[0];
            return match self.version_cmp(a, b) {
                None => {
                    // Versions are concurrent.
                    if a < b { Frontier::from_sorted(&[a, b]) }
                    else { Frontier::from_sorted(&[b, a]) }
                }
                Some(Ordering::Equal) | Some(Ordering::Less) => Frontier::new_1(b),
                Some(Ordering::Greater) => Frontier::new_1(a)
            };
        }
        // Could do some more fast path things here using version_contains_time() if v_1 / v_2.len == 1.

        let first_v = v_1[0].min(v_2[0]);

        let mut result_rev = smallvec![];

        let iter = v_1.iter().copied().chain(v_2.iter().copied());
        self.find_dominators_full_internal(iter, first_v, |v, dom| {
            if dom {
                result_rev.push(v);
            }
        });

        result_rev.reverse();
        Frontier(result_rev)
    }

    fn find_dominators_full_internal<F, I>(&self, versions_iter: I, stop_at_shadow: usize, mut visit: F)
        where F: FnMut(LV, bool), I: Iterator<Item=LV>
    {
        if let Some(max_size) = versions_iter.size_hint().1 {
            if max_size <= 1 {
                // All items are dominators.
                for v in versions_iter {
                    visit(v, true);
                }
                return;
            }
        }

        // Using the LSB in the data to encode whether this version was an input to the function.
        // We hit all the "normal" versions before the inputs.
        fn enc_input(v: LV) -> usize { v << 1 }
        fn enc_normal(v: LV) -> usize { (v << 1) + 1 }
        fn dec(v_enc: usize) -> (bool, LV) {
            (v_enc % 2 == 0, v_enc >> 1)
        }

        let mut queue: BinaryHeap<usize> = versions_iter.map(|v| {
            if v >= usize::MAX / 2 { panic!("Cannot handle version beyond usize::MAX/2"); }
            enc_input(v)
        }).collect();
        let mut inputs_remaining = queue.len();

        let mut last_emitted = usize::MAX;

        while let Some(v_enc) = queue.pop() {
            // dbg!(&queue, v_enc);
            let (is_input, v) = dec(v_enc);

            if is_input {
                visit(v, true);
                last_emitted = v;
                inputs_remaining -= 1;
            }

            let e = self.entries.find_packed(v);

            if stop_at_shadow != usize::MAX && e.shadow <= stop_at_shadow {
                break;
            }

            // println!("Pop {v} {is_input}");

            while let Some(&v2_enq) = queue.peek() {
                let (is_input2, v2) = dec(v2_enq);
                if v2 < e.span.start { break; } // We don't need is_input2 yet...
                // println!("Peek {v2} {is_input2}");
                // if v2 < (e.span.start * 2) { break; }
                queue.pop();

                if is_input2 {
                    // TODO: Not sure what to do if the input data has duplicates. I think it makes
                    // the most sense to transparently uniq() the output stream but ??.
                    if last_emitted != v2 {
                        visit(v2, false);
                        last_emitted = v2;
                    }
                    inputs_remaining -= 1;
                }
            }
            if inputs_remaining == 0 { break; }

            for p in e.parents.iter() {
                // dbg!(p);
                queue.push(enc_normal(*p));
            }
        }
    }

    /// Given some disparate set of versions, figure out which versions are dominators - ie, the
    /// set of versions which "contains" the entire set of versions in their transitive dependency
    /// graph.
    ///
    /// This function might be better written to output an iterator.
    pub fn find_dominators_full<F, I>(&self, versions_iter: I, visit: F)
        where F: FnMut(LV, bool), I: Iterator<Item=LV>
    {
        self.find_dominators_full_internal(versions_iter, usize::MAX, visit);
    }

    // /// Find dominators on an unsorted set of versions
    // pub fn find_dominators_unsorted_rev(&self, versions: &[LV]) -> SmallVec<[LV; 2]> {
    //     if versions.len() <= 1 {
    //         return versions.into();
    //     }
    //
    //     let mut result = smallvec![];
    //     self.find_dominators_full(versions.iter().copied(), |v, is_input| {
    //         if is_input {
    //             result.push(v);
    //         }
    //     });
    //
    //     result
    // }
    //
    // pub fn find_dominators_unsorted(&self, versions: &[LV]) -> Frontier {
    //     let mut result = self.find_dominators_unsorted_rev(versions);
    //     result.reverse();
    //     Frontier(result)
    // }

    /// Given 2 versions, return a version which contains all the operations in both.
    ///
    /// TODO: This needs unit tests.
    pub fn version_union(&self, a: &[LV], b: &[LV]) -> Frontier {
        let mut result = smallvec![];
        // Using find_dominators_full to avoid a sort() here. Not sure if thats worth it though.
        self.find_dominators_full(
            a.iter().copied().chain(b.iter().copied()),
            |v, is_dom| {
                if is_dom {
                    result.push(v);
                }
            }
        );
        result.reverse();
        Frontier(result)
    }
}

#[cfg(test)]
pub mod test {
    use std::ops::Range;
    use smallvec::smallvec;
    use rle::{AppendRle, MergableSpan};

    use crate::causalgraph::graph::*;
    use crate::causalgraph::graph::tools::DiffFlag::*;
    use crate::causalgraph::graph::tools::{DiffFlag, DiffResult};
    use crate::dtrange::DTRange;
    use crate::rle::RleVec;
    use crate::{Frontier, LV};
    use crate::frontier::debug_assert_sorted;

    // The conflict finder can also be used as an overly complicated diff function. Check this works
    // (This is mostly so I can reuse a bunch of tests).
    fn diff_via_conflicting(graph: &Graph, a: &[LV], b: &[LV]) -> DiffResult {
        let mut only_a = smallvec![];
        let mut only_b = smallvec![];

        graph.find_conflicting(a, b, |span, flag| {
            // dbg!((span, flag));
            let target = match flag {
                OnlyA => { &mut only_a }
                OnlyB => { &mut only_b }
                Shared => { return; }
            };
            // dbg!((ord_start, ord_end));

            target.push_reversed_rle(span);
        });

        (only_a, only_b)
    }

    #[derive(Debug, Eq, PartialEq)]
    pub struct ConflictFull {
        pub(crate) common_branch: Frontier,
        pub(crate) spans: Vec<(DTRange, DiffFlag)>,
    }

    fn push_rev_rle(list: &mut Vec<(DTRange, DiffFlag)>, span: DTRange, flag: DiffFlag) {
        if let Some((last_span, last_flag)) = list.last_mut() {
            if span.can_append(last_span) && flag == *last_flag {
                last_span.prepend(span);
                return;
            }
        }
        list.push((span, flag));
    }

    fn find_conflicting(graph: &Graph, a: &[LV], b: &[LV]) -> ConflictFull {
        let mut spans_fast = Vec::new();
        let mut spans_slow = Vec::new();

        let common_branch_fast = graph.find_conflicting(a, b, |span, flag| {
            debug_assert!(!span.is_empty());
            // spans_fast.push((span, flag));
            push_rev_rle(&mut spans_fast, span, flag);
        });
        let common_branch_slow = graph.find_conflicting_slow(a, b, |span, flag| {
            debug_assert!(!span.is_empty());
            // spans_slow.push((span, flag));
            push_rev_rle(&mut spans_slow, span, flag);
        });
        assert_eq!(spans_fast, spans_slow);
        assert_eq!(common_branch_fast, common_branch_slow);

        ConflictFull {
            common_branch: common_branch_slow,
            spans: spans_slow,
        }
    }

    fn assert_conflicting(graph: &Graph, a: &[LV], b: &[LV], expect_spans: &[(Range<usize>, DiffFlag)], expect_common: &[LV]) {
        let expect: Vec<(DTRange, DiffFlag)> = expect_spans
            .iter()
            .rev()
            .map(|(r, flag)| (r.clone().into(), *flag))
            .collect();
        let actual = find_conflicting(graph, a, b);
        assert_eq!(actual.common_branch.as_ref(), expect_common);
        assert_eq!(actual.spans, expect);

        #[cfg(feature="gen_test_data")] {
            #[derive(Serialize)]
            #[derive(Clone)]
            struct Test<'a> {
                hist: Vec<GraphEntrySimple>,
                a: &'a [LV],
                b: &'a [LV],
                expect_spans: &'a [(Range<usize>, DiffFlag)],
                expect_common: &'a [LV],
            }

            let t = Test {
                hist: graph.iter().collect(), a, b, expect_spans, expect_common
            };

            let _p: Vec<_> = graph.iter().collect();
            use std::io::Write;
            let mut f = std::fs::File::options()
                .write(true)
                .append(true)
                .create(true)
                .open("test_data/causal_graph/conflicting.json").unwrap();
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
        }
    }

    fn assert_version_contains_time(graph: &Graph, frontier: &[LV], target: LV, expected: bool) {
        #[cfg(feature="gen_test_data")] {
            #[cfg_attr(feature = "serde", derive(Serialize))]
            #[derive(Clone, Debug)]
            struct Test<'a> {
                hist: Vec<GraphEntrySimple>,
                frontier: &'a [LV],
                target: isize,
                expected: bool,
            }

            let t = Test {
                hist: graph.iter().collect(), frontier, target: target as _, expected
            };

            let _p: Vec<_> = graph.iter().collect();
            use std::io::Write;
            let mut f = std::fs::File::options()
                .write(true)
                .append(true)
                .create(true)
                .open("test_data/causal_graph/version_contains.json").unwrap();
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
        }

        assert_eq!(graph.frontier_contains_version(frontier, target), expected);
    }

    fn assert_diff_eq(graph: &Graph, a: &[LV], b: &[LV], expect_a: &[DTRange], expect_b: &[DTRange]) {
        #[cfg(feature="gen_test_data")] {
            #[cfg_attr(feature = "serde", derive(Serialize))]
            #[derive(Clone)]
            struct Test<'a> {
                hist: Vec<GraphEntrySimple>,
                a: &'a [LV],
                b: &'a [LV],
                expect_a: &'a [DTRange],
                expect_b: &'a [DTRange],
            }

            let t = Test {
                hist: graph.iter().collect(),
                a,
                b,
                expect_a,
                expect_b
            };

            let _p: Vec<_> = graph.iter().collect();
            use std::io::Write;
            let mut f = std::fs::File::options()
                .write(true)
                .append(true)
                .create(true)
                .open("test_data/causal_graph/diff.json").unwrap();
            writeln!(f, "{}", serde_json::to_string(&t).unwrap()).unwrap();
        }

        let slow_result = graph.diff_slow(a, b);
        let fast_result = graph.diff_rev(a, b);
        let c_result = diff_via_conflicting(graph, a, b);

        assert_eq!(slow_result.0.as_slice(), expect_a);
        assert_eq!(slow_result.1.as_slice(), expect_b);

        // dbg!(&slow_result, &fast_result);
        assert_eq!(slow_result, fast_result);
        // dbg!(&slow_result, &c_result);
        assert_eq!(slow_result, c_result);

        for &(branch, spans, other) in &[(a, expect_a, b), (b, expect_b, a)] {
            for o in spans {
                assert_version_contains_time(graph, branch, o.start, true);
                if o.len() > 1 {
                    assert_version_contains_time(graph, branch, o.last(), true);
                }
            }

            if branch.len() == 1 {
                // dbg!(&other, branch[0], &spans);
                let expect = spans.is_empty();
                assert_version_contains_time(graph, other, branch[0], expect);
            }
        }

        // TODO: Could add extra checks for each specific version in here too. Eh!
    }

    pub(crate) fn fancy_graph() -> Graph {
        let g = Graph::from_simple_items(&[
            GraphEntrySimple { span: (0..3).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (3..6).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (6..9).into(), parents: Frontier::from_sorted(&[1, 4]) },
            GraphEntrySimple { span: (9..11).into(), parents: Frontier::from_sorted(&[2, 8]) },
        ]);

        assert_eq!(g.entries.0.len(), 4);
        assert_eq!(g.entries[0].shadow, 0);
        assert_eq!(g.entries[1].shadow, 3);
        assert_eq!(g.entries[2].shadow, 6);
        assert_eq!(g.entries[3].shadow, 6);

        g.dbg_check(true);
        g
    }

    #[test]
    fn common_item_smoke_test() {
        let graph = fancy_graph();

        for t in 0..=9 {
            // dbg!(t);
            // The same item should never conflict with itself.
            assert_conflicting(&graph, &[t], &[t], &[], &[t]);
        }
        assert_conflicting(&graph, &[5, 6], &[5, 6], &[], &[5, 6]);

        assert_conflicting(&graph, &[1], &[2], &[(2..3, OnlyB)], &[1]);
        assert_conflicting(&graph, &[0], &[2], &[(1..3, OnlyB)], &[0]);
        assert_conflicting(&graph, &[], &[], &[], &[]);
        assert_conflicting(&graph, &[], &[0], &[(0..1, OnlyB)], &[]);
        assert_conflicting(&graph, &[], &[2], &[(0..3, OnlyB)], &[]);

        assert_conflicting(&graph, &[2], &[3], &[(0..3, OnlyA), (3..4, OnlyB)], &[]); // 0,1,2 and 3.
        assert_conflicting(&graph, &[1, 4], &[4], &[(0..2, OnlyA), (3..5, Shared)], &[]); // 0,1,2 and 3.
        assert_conflicting(&graph, &[6], &[2], &[(0..2, Shared), (2..3, OnlyB), (3..5, OnlyA), (6..7, OnlyA)], &[]);
        assert_conflicting(&graph, &[6], &[5], &[(0..2, OnlyA), (3..5, Shared), (5..6, OnlyB), (6..7, OnlyA)], &[]); // 6 includes 1, 0.
        assert_conflicting(&graph, &[5, 6], &[5], &[(0..2, OnlyA), (3..6, Shared), (6..7, OnlyA)], &[]);
        assert_conflicting(&graph, &[5, 6], &[2], &[(0..2, Shared), (2..3, OnlyB), (3..7, OnlyA)], &[]);
        assert_conflicting(&graph, &[2, 6], &[5], &[(0..3, OnlyA), (3..5, Shared), (5..6, OnlyB), (6..7, OnlyA)], &[]);
        assert_conflicting(&graph, &[9], &[10], &[(10..11, OnlyB)], &[9]);
        assert_conflicting(&graph, &[6], &[7], &[(7..8, OnlyB)], &[6]);

        // This looks weird, but its right because 9 shares the same parents.
        assert_conflicting(&graph, &[9], &[2, 8], &[(9..10, OnlyA)], &[2, 8]);

        // Everything! Just because we need to rebase operation 8 on top of 7, and can't produce
        // that without basically all of time. Hopefully this doesn't come up a lot in practice.
        assert_conflicting(&graph, &[9], &[2, 7], &[(0..5, Shared), (6..8, Shared), (8..10, OnlyA)], &[]);
    }

    #[test]
    fn branch_contains_smoke_test() {
        // let mut doc = ListCRDT::new();
        // assert!(doc.txns.branch_contains_order(&doc.frontier, ROOT_TIME_X));
        //
        // doc.get_or_create_agent_id("a");
        // doc.local_insert(0, 0, "S".into()); // Shared history.
        // assert!(doc.txns.branch_contains_order(&doc.frontier, ROOT_TIME_X));
        // assert!(doc.txns.branch_contains_order(&doc.frontier, 0));
        // assert!(!doc.txns.branch_contains_order(&[ROOT_TIME_X], 0));

        let parents = fancy_graph();

        assert_version_contains_time(&parents, &[], 0, false);
        assert_version_contains_time(&parents, &[0], 0, true);

        assert_version_contains_time(&parents, &[2], 0, true);
        assert_version_contains_time(&parents, &[2], 1, true);
        assert_version_contains_time(&parents, &[2], 2, true);

        assert_version_contains_time(&parents, &[0], 1, false);
        assert_version_contains_time(&parents, &[1], 2, false);

        assert_version_contains_time(&parents, &[8], 0, true);
        assert_version_contains_time(&parents, &[8], 1, true);
        assert_version_contains_time(&parents, &[8], 2, false);
        assert_version_contains_time(&parents, &[8], 5, false);

        assert_version_contains_time(&parents, &[1,4], 0, true);
        assert_version_contains_time(&parents, &[1,4], 1, true);
        assert_version_contains_time(&parents, &[1,4], 2, false);
        assert_version_contains_time(&parents, &[1,4], 5, false);

        assert_version_contains_time(&parents, &[9], 2, true);
        assert_version_contains_time(&parents, &[9], 1, true);
        assert_version_contains_time(&parents, &[9], 0, true);
    }

    fn check_dominators(graph: &Graph, input: &[LV], expected_yes: &[LV]) {
        debug_assert_sorted(input);
        debug_assert_sorted(expected_yes);

        let expected_no: Vec<_> = input.iter().filter(|v| !expected_yes.contains(v)).copied().collect();
        debug_assert_sorted(expected_no.as_slice());
        assert_eq!(input.len(), expected_yes.len() + expected_no.len());

        assert_eq!(graph.find_dominators(input).as_ref(), expected_yes);

        let mut actual_yes = vec![];
        let mut actual_no = vec![];
        graph.find_dominators_full(input.iter().copied(), |v, dom| {
            if dom { actual_yes.push(v); }
            else { actual_no.push(v); }
        });
        actual_yes.reverse();
        actual_no.reverse();

        assert_eq!(actual_yes, expected_yes);
        assert_eq!(actual_no, expected_no);

        // This is a bit dirty, but no harm in it!
        for split in 0..=input.len() {
            let (a_raw, b_raw) = input.split_at(split);
            // So unfortunately (for our purposes) find_dominators_2 assumes the two passed arrays
            // are already trimmed to their dominators.
            // IF ONLY we had a method to help!!
            let a = graph.find_dominators(a_raw);
            let b = graph.find_dominators(b_raw);
            let fwd = graph.find_dominators_2(a.as_ref(), b.as_ref());
            let rev = graph.find_dominators_2(b.as_ref(), a.as_ref());
            assert_eq!(fwd.as_ref(), expected_yes);
            assert_eq!(rev.as_ref(), expected_yes);
        }
    }

    #[test]
    fn dominator_smoke_test() {
        let parents = fancy_graph();

        check_dominators(&parents, &[0,1,2,3,4,5,6,7,8,9,10], &[5, 10]);
        check_dominators(&parents, &[10], &[10]);

        check_dominators(&parents, &[5, 6], &[5, 6]);
        check_dominators(&parents, &[5, 9], &[5, 9]);
        check_dominators(&parents, &[4, 9], &[9]);
        check_dominators(&parents, &[1, 2], &[2]);
        check_dominators(&parents, &[0, 2], &[2]);
        check_dominators(&parents, &[0, 10], &[10]);
        check_dominators(&parents, &[], &[]);
        check_dominators(&parents, &[2], &[2]);
        check_dominators(&parents, &[1, 4], &[1, 4]);
        check_dominators(&parents, &[9, 10], &[10]);
        check_dominators(&parents, &[2, 8, 9], &[9]);
        check_dominators(&parents, &[2, 7, 9], &[9]);
        check_dominators(&parents, &[6, 7], &[7]);
        check_dominators(&parents, &[0], &[0]);
    }

    #[test]
    fn dominator_duplicates() {
        let parents = fancy_graph();
        assert_eq!(parents.find_dominators(&[1,1,1]).as_ref(), &[1]);
        assert_eq!(parents.version_union(&[1], &[1]).as_ref(), &[1]);

        let mut seen_1 = false;
        parents.find_dominators_full((&[1,1,1]).iter().copied(), |_v, _d| {
            if !seen_1 { seen_1 = true; }
            else { panic!("Duplicate version!"); }
        });
    }

    #[test]
    fn diff_for_flat_txns() {
        // Regression.

        // 0 |
        // | 1
        // 2

        let graph = Graph::from_simple_items(&[
            GraphEntrySimple { span: (0..1).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (1..2).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (2..3).into(), parents: Frontier::from_sorted(&[0]) },
        ]);

        graph.dbg_check(true);

        assert_diff_eq(&graph, &[2], &[], &[(2..3).into(), (0..1).into()], &[]);
        assert_diff_eq(&graph, &[2], &[1], &[(2..3).into(), (0..1).into()], &[(1..2).into()]);
    }

    #[test]
    fn diff_three_root_txns() {
        // Regression.

        // 0 | |
        //   1 |
        //     2
        let graph = Graph::from_simple_items(&[
            GraphEntrySimple { span: (0..1).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (1..2).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (2..3).into(), parents: Frontier::root() },
        ]);

        graph.dbg_check(true);

        assert_diff_eq(&graph, &[0], &[0, 1], &[], &[(1..2).into()]);

        for time in [0, 1, 2] {
            assert_diff_eq(&graph, &[time], &[], &[(time..time+1).into()], &[]);
            assert_diff_eq(&graph, &[], &[time], &[], &[(time..time+1).into()]);
        }

        assert_diff_eq(&graph, &[], &[0, 1], &[], &[(0..2).into()]);
        assert_diff_eq(&graph, &[0], &[1], &[(0..1).into()], &[(1..2).into()]);
    }

    #[test]
    fn diff_shadow_bubble() {
        // regression

        // 0,1,2   |
        //      \ 3,4
        //       \ /
        //        5,6
        let graph = Graph::from_simple_items(&[
            GraphEntrySimple { span: (0..3).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (3..5).into(), parents: Frontier::root() },
            GraphEntrySimple { span: (5..6).into(), parents: Frontier::from_sorted(&[2, 4]) },
        ]);

        graph.dbg_check(true);

        assert_diff_eq(&graph, &[4], &[5], &[], &[(5..6).into(), (0..3).into()]);
        assert_diff_eq(&graph, &[4], &[], &[(3..5).into()], &[]);
    }

    #[test]
    fn diff_common_branch_is_ordered() {
        // Regression
        // 0 1
        // |x|
        // 2 3
        let parents = Graph::from_simple_items(&[
            GraphEntrySimple { span: (0..1).into(), parents: Frontier::from_sorted(&[]) },
            GraphEntrySimple { span: (1..2).into(), parents: Frontier::from_sorted(&[]) },
            GraphEntrySimple { span: (2..3).into(), parents: Frontier::from_sorted(&[0, 1]) },
            GraphEntrySimple { span: (3..4).into(), parents: Frontier::from_sorted(&[0, 1]) },
        ]);
        parents.dbg_check(true);

        assert_version_contains_time(&parents, &[2], 3, false);
        assert_version_contains_time(&parents, &[3], 2, false);
        assert_diff_eq(&parents, &[2], &[3], &[(2..3).into()], &[(3..4).into()]);
    }


    // #[test]
    // fn diff_smoke_test() {
    //     let mut doc1 = ListCRDT::new();
    //     assert_diff_eq(&doc1.history, &doc1.frontier, &doc1.frontier, &[], &[]);
    //
    //     doc1.get_or_create_agent_id("a");
    //     doc1.local_insert(0, 0, "S".into()); // Shared history.
    //
    //     let mut doc2 = ListCRDT::new();
    //     doc2.get_or_create_agent_id("b");
    //     doc1.replicate_into(&mut doc2); // "S".
    //
    //     // Ok now make some concurrent history.
    //     doc1.local_insert(0, 1, "aaa".into());
    //     let b1 = doc1.frontier.clone();
    //
    //     assert_diff_eq(&doc1.txns, &b1, &b1, &[], &[]);
    //     assert_diff_eq(&doc1.txns, &[ROOT_TIME_X], &[ROOT_TIME_X], &[], &[]);
    //     // dbg!(&doc1.frontier);
    //
    //     // There are 4 items in doc1 - "Saaa".
    //     // dbg!(&doc1.frontier); // [3]
    //     assert_diff_eq(&doc1.txns, &[1], &[3], &[], &[2..4]);
    //
    //     doc2.local_insert(0, 1, "bbb".into());
    //
    //     doc2.replicate_into(&mut doc1);
    //
    //     // doc1 has "Saaabbb".
    //
    //     // dbg!(doc1.diff(&b1, &doc1.frontier));
    //
    //     assert_diff_eq(&doc1.txns, &b1, &doc1.frontier, &[], &[4..7]);
    //     assert_diff_eq(&doc1.txns, &[3], &[6], &[1..4], &[4..7]);
    //     assert_diff_eq(&doc1.txns, &[2], &[5], &[1..3], &[4..6]);
    //
    //     // doc1.replicate_into(&mut doc2); // Also "Saaabbb" but different txns.
    //     // dbg!(&doc1.txns, &doc2.txns);
    // }

    // fn root_id() -> RemoteId {
    //     RemoteId {
    //         agent: "ROOT".into(),
    //         seq: u32::MAX
    //     }
    // }
    //
    // pub fn complex_multientry_doc() -> ListCRDT {
    //     let mut doc = ListCRDT::new();
    //     doc.get_or_create_agent_id("a");
    //     doc.get_or_create_agent_id("b");
    //
    //     assert_eq!(doc.frontier.as_slice(), &[ROOT_TIME_X]);
    //
    //     doc.local_insert(0, 0, "aaa".into());
    //
    //     assert_eq!(doc.frontier.as_slice(), &[2]);
    //
    //     // Need to do this manually to make the change concurrent.
    //     doc.apply_remote_txn(&RemoteTxn {
    //         id: RemoteId { agent: "b".into(), seq: 0 },
    //         parents: smallvec![root_id()],
    //         ops: smallvec![RemoteCRDTOp::Ins {
    //             origin_left: root_id(),
    //             origin_right: root_id(),
    //             len: 2,
    //             content_known: true,
    //         }],
    //         ins_content: "bb".into(),
    //     });
    //
    //     assert_eq!(doc.frontier.as_slice(), &[2, 4]);
    //
    //     // And need to do this manually to make the change not merge time.
    //     doc.apply_remote_txn(&RemoteTxn {
    //         id: RemoteId { agent: "a".into(), seq: 3 },
    //         parents: smallvec![RemoteId { agent: "a".into(), seq: 2 }],
    //         ops: smallvec![RemoteCRDTOp::Ins {
    //             origin_left: RemoteId { agent: "a".into(), seq: 2 },
    //             origin_right: root_id(),
    //             len: 2,
    //             content_known: true,
    //         }],
    //         ins_content: "AA".into(),
    //     });
    //
    //     assert_eq!(doc.frontier.as_slice(), &[4, 6]);
    //
    //     if let Some(ref text) = doc.text_content {
    //         assert_eq!(text, "aaaAAbb");
    //     }
    //
    //     doc
    // }

    // #[test]
    // fn diff_with_multiple_entries() {
    //     let doc = complex_multientry_doc();
    //
    //     // dbg!(&doc.txns);
    //     // dbg!(doc.diff(&smallvec![6], &smallvec![]));
    //     // dbg!(&doc);
    //
    //     assert_diff_eq(&doc.txns, &[6], &[ROOT_TIME_X], &[5..7, 0..3], &[]);
    //     assert_diff_eq(&doc.txns, &[6], &[4], &[5..7, 0..3], &[3..5]);
    //     assert_diff_eq(&doc.txns, &[4, 6], &[ROOT_TIME_X], &[0..7], &[]);
    // }

}