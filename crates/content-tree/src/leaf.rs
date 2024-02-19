use std::mem::take;
use std::ptr::NonNull;

use rle::Searchable;

use super::*;

impl<E: ContentTraits, I: TreeMetrics<E>, const IE: usize, const LE: usize> NodeLeaf<E, I, IE, LE> {
    // Note this doesn't return a Pin<Box<Self>> like the others. At the point of creation, there's
    // no reason for this object to be pinned. (Is that a bad idea? I'm not sure.)
    pub(crate) unsafe fn new(next: Option<NonNull<Self>>) -> Self {
        Self::new_with_parent(ParentPtr::Root(NonNull::dangling()), next)
    }

    pub(crate) fn new_with_parent(parent: ParentPtr<E, I, IE, LE>, next: Option<NonNull<Self>>) -> Self {
        Self {
            parent,
            data: [E::default(); LE],
            num_entries: 0,
            _pin: PhantomPinned,
            next,
        }
    }

    // pub fn find2(&self, loc: CRDTLocation) -> (ClientSeq, Option<usize>) {
    //     let mut raw_pos: ClientSeq = 0;

    //     for i in 0..NUM_ENTRIES {
    //         let entry = self.data[i];
    //         if entry.is_invalid() { break; }

    //         if entry.loc.client == loc.client && entry.get_seq_range().contains(&loc.seq) {
    //             if entry.len > 0 {
    //                 raw_pos += loc.seq - entry.loc.seq;
    //             }
    //             return (raw_pos, Some(i));
    //         } else {
    //             raw_pos += entry.get_text_len()
    //         }
    //     }
    //     (raw_pos, None)
    // }

    /// Find a given offset within the node.
    ///
    /// Returns (index, offset within entry)
    ///
    /// TODO: Add a parameter for figuring out the offset at content length, and pass that in
    /// from the ContentLength trait.
    pub fn find_offset<F>(&self, mut offset: usize, stick_end: bool, entry_to_num: F) -> Option<(usize, usize)>
        where F: Fn(E) -> usize {
        for i in 0..self.len_entries() {
            // if offset == 0 {
            //     return Some((i, 0));
            // }

            let entry: E = self.data[i];
            // if !entry.is_valid() { break; }

            // let text_len = entry.content_len();
            let entry_len = entry_to_num(entry);
            if offset < entry_len || (stick_end && entry_len == offset) {
                // Found it.
                return Some((i, offset));
            } else {
                offset -= entry_len
            }
        }

        if offset == 0 { // Special case for the first inserted element - we may never enter the loop.
            Some((self.len_entries(), 0))
        } else { None }
    }

    pub fn next_leaf(&self) -> Option<NonNull<Self>> {
        self.next
    }

    pub fn prev_leaf(&self) -> Option<NonNull<Self>> {
        self.adjacent_leaf_by_traversal(false)
    }

    pub(crate) fn adjacent_leaf_by_traversal(&self, direction_forward: bool) -> Option<NonNull<Self>> {
        // TODO: Remove direction_forward here.

        // println!("** traverse called {:?} {}", self, traverse_next);
        // idx is 0. Go up as far as we can until we get to an index that has room, or we hit the
        // root.
        let mut parent = self.parent;
        let mut node_ptr = NodePtr::Leaf(unsafe { NonNull::new_unchecked(self as *const _ as *mut _) });

        loop {
            match parent {
                ParentPtr::Root(_) => { return None; },
                ParentPtr::Internal(n) => {
                    let node_ref = unsafe { n.as_ref() };
                    // Time to find ourself up this tree.
                    let idx = node_ref.find_child(node_ptr).unwrap();
                    // println!("found myself at {}", idx);

                    let next_idx: Option<usize> = if direction_forward {
                        let next_idx = idx + 1;
                        // This would be much cleaner if I put a len field in NodeInternal instead.
                        // TODO: Consider using node_ref.count_children() instead of this mess.
                        if (next_idx < IE) && node_ref.children[next_idx].is_some() {
                            Some(next_idx)
                        } else { None }
                    } else if idx > 0 {
                        Some(idx - 1)
                    } else { None };
                    // println!("index {:?}", next_idx);

                    if let Some(next_idx) = next_idx {
                        // Whew - now we can descend down from here.
                        // println!("traversing laterally to {}", next_idx);
                        node_ptr = unsafe { node_ref.children[next_idx].as_ref().unwrap().as_ptr() };
                        break;
                    } else {
                        // idx is 0. Keep climbing that ladder!
                        node_ptr = NodePtr::Internal(unsafe { NonNull::new_unchecked(node_ref as *const _ as *mut _) });
                        parent = node_ref.parent;
                    }
                }
            }
        }

        // Now back down. We don't need idx here because we just take the first / last item in each
        // node going down the tree.
        loop {
            // println!("nodeptr {:?}", node_ptr);
            match node_ptr {
                NodePtr::Internal(n) => {
                    let node_ref = unsafe { n.as_ref() };
                    let next_idx = if direction_forward {
                        0
                    } else {
                        let num_children = node_ref.count_children();
                        assert!(num_children > 0);
                        num_children - 1
                    };
                    node_ptr = unsafe { node_ref.children[next_idx].as_ref().unwrap().as_ptr() };
                },
                NodePtr::Leaf(n) => {
                    // Finally.
                    return Some(n);
                }
            }
        }
    }

    // pub(super) fn actually_count_entries(&self) -> usize {
    //     self.data.iter()
    //     .position(|e| e.loc.client == CLIENT_INVALID)
    //     .unwrap_or(NUM_ENTRIES)
    // }
    pub fn len_entries(&self) -> usize {
        self.num_entries as usize
    }

    pub fn as_slice(&self) -> &[E] {
        &self.data[0..self.num_entries as usize]
    }

    // Recursively (well, iteratively) ascend and update all the counts along
    // the way up. TODO: Move this - This method shouldn't be in NodeLeaf.
    pub fn update_parent_count(&mut self, amt: I::Update) {
        if amt == I::Update::default() { return; }

        let mut child = NodePtr::Leaf(unsafe { NonNull::new_unchecked(self) });
        let mut parent = self.parent;

        loop {
            match parent {
                ParentPtr::Root(mut r) => {
                    unsafe {
                        I::update_offset_by_marker(&mut r.as_mut().count, &amt);
                        // r.as_mut().count = r.as_ref().count.wrapping_add(amt as usize); }
                    }
                    break;
                },
                ParentPtr::Internal(mut n) => {
                    let idx = unsafe { n.as_mut() }.find_child(child).unwrap();
                    let c = &mut unsafe { n.as_mut() }.metrics[idx];
                    // :(
                    I::update_offset_by_marker(c, &amt);
                    // *c = c.wrapping_add(amt as u32);

                    // And recurse.
                    child = NodePtr::Internal(n);
                    parent = unsafe { n.as_mut() }.parent;
                },
            };
        }
    }

    pub fn flush_metric_update(&mut self, marker: &mut I::Update) {
        // println!("flush {:?}", marker);
        let amt = take(marker);
        self.update_parent_count(amt);
    }

    pub fn has_root_as_parent(&self) -> bool {
        self.parent.is_root()
    }

    pub fn count_items(&self) -> I::Value {
        if I::CAN_COUNT_ITEMS {
            // Optimization using the index. TODO: check if this is actually faster.
            match self.parent {
                ParentPtr::Root(root) => {
                    unsafe { root.as_ref() }.count
                }
                ParentPtr::Internal(node) => {
                    let child = NodePtr::Leaf(unsafe { NonNull::new_unchecked(self as *const _ as *mut _) });
                    let idx = unsafe { node.as_ref() }.find_child(child).unwrap();
                    unsafe { node.as_ref() }.metrics[idx]
                }
            }
        } else {
            // Count items the boring way. Hopefully this will optimize tightly.
            let mut val = I::Value::default();
            for elem in self.data[..self.num_entries as usize].iter() {
                I::increment_offset(&mut val, elem);
            }
            val
        }
    }

    /// Remove a single item from the node
    pub fn splice_out(&mut self, idx: usize) {
        debug_assert!(idx < self.num_entries as usize);
        self.data.copy_within(idx + 1..self.num_entries as usize, idx);
        self.num_entries -= 1;
    }

    pub fn clear_all(&mut self) {
        // self.data[0..self.num_entries as usize].fill(E::default());
        self.num_entries = 0;
    }

    pub fn unsafe_cursor_at_start(&self) -> UnsafeCursor<E, I, IE, LE> {
        UnsafeCursor::new(
            unsafe { NonNull::new_unchecked(self as *const _ as *mut _) },
            0,
            0
        )
    }
    
    // pub fn cursor_at_start<'a, 'b: 'a>(&'a self, tree: &'b ContentTreeRaw<E, I, IE, LE>) -> Cursor<E, I, IE, LE> {
    //     // This is safe because you can only reference a leaf while you immutably borrow a
    //     // content-tree. The lifetime of the returned cursor should match self.
    //     unsafe { Cursor::unchecked_from_raw(tree, self.unsafe_cursor_at_start()) }
    // }
}

impl<E: ContentTraits + Searchable, I: TreeMetrics<E>, const IE: usize, const LE: usize> NodeLeaf<E, I, IE, LE> {
    pub fn find(&self, loc: E::Item) -> Option<UnsafeCursor<E, I, IE, LE>> {
        for i in 0..self.len_entries() {
            let entry: E = self.data[i];

            if let Some(offset) = entry.get_offset(loc) {
                debug_assert!(offset < entry.len());
                // let offset = if entry.is_insert() { entry_offset } else { 0 };

                return Some(UnsafeCursor::new(
                    unsafe { NonNull::new_unchecked(self as *const _ as *mut _) },
                    i,
                    offset
                ))
            }
        }
        None
    }
}
