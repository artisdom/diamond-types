use super::*;
use std::mem::{self, MaybeUninit};

impl NodeInternal {
    // pub(super) unsafe fn new() -> Self {
    //     Self::new_with_parent(ParentPtr::Root(NonNull::dangling()))
    // }

    pub(super) fn new_with_parent(parent: ParentPtr) -> Self {
        // From the example in the docs:
        // https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#initializing-an-array-element-by-element
        let mut children: [MaybeUninit<(ItemCount, Option<Pin<Box<Node>>>)>; MAX_CHILDREN] = unsafe {
            MaybeUninit::uninit().assume_init() // Safe because `MaybeUninit`s don't require init.
        };
        for elem in &mut children[..] {
            *elem = MaybeUninit::new((0, None));
        }
        Self {
            parent,
            data: unsafe {
                mem::transmute::<_, [(ItemCount, Option<Pin<Box<Node>>>); MAX_CHILDREN]>(children)
            },
            _pin: PhantomPinned,
            _drop: PrintDropInternal,
        }
    }

    pub(super) fn get_child(&self, raw_pos: u32, stick_end: bool) -> Option<(u32, Pin<&Node>)> {
        let mut offset_remaining = raw_pos;

        for (count, elem) in self.data.iter() {
            let count = *count;
            if let Some(elem) = elem.as_ref() {
                if offset_remaining < count || (stick_end && offset_remaining == count) {
                    // let elem_box = elem.unwrap();
                    return Some((offset_remaining, elem.as_ref()))
                } else {
                    offset_remaining -= count;
                    // And continue.
                }
            } else { return None }
        }
        None
    }

    // pub(super) fn get_child_mut(&mut self, raw_pos: u32) -> Option<(u32, Pin<&mut Node>)> {
    //     let mut offset_remaining = raw_pos;

    //     self.data.iter_mut().find_map(|(count, elem)| {
    //         if let Some(elem) = elem.as_mut() {
    //             if offset_remaining < *count {
    //                 Some((offset_remaining, elem.as_mut()))
    //             } else {
    //                 offset_remaining -= *count;
    //                 None
    //             }
    //         } else { None }
    //     })
    // }

    /// Insert a new item in the tree. This DOES NOT update the child counts in
    /// the parents. (So the tree will be in an invalid state after this has been called.)
    pub(super) fn splice_in(&mut self, idx: usize, count: u32, elem: Pin<Box<Node>>) {
        let mut buffer = (count, Some(elem));
        for i in idx..MAX_CHILDREN {
            mem::swap(&mut buffer, &mut self.data[i]);
            if buffer.1.is_none() { break; }
        }
        debug_assert!(buffer.1.is_none(), "tried to splice in to a node that was full");
    }

    pub(super) fn count_children(&self) -> usize {
        self.data.iter()
        .position(|(_, c)| c.is_none())
        .unwrap_or(MAX_CHILDREN)
    }

    pub(super) fn find_child(&self, child: NodePtr) -> Option<usize> {
        self.data.iter()
        .position(|(_, c)| c.as_ref()
            .map_or(false, |n| n.ptr_eq(child))
        )
    }

    unsafe fn to_parent_ptr(&mut self) -> ParentPtr {
        ParentPtr::Internal(NonNull::new_unchecked(self))
    }
}
