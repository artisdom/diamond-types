// This file contains an implementation of Eq / PartialEq for OpLog. The implementation is quite
// complex because:
//
// - Operation logs don't have a canonical ordering (because of bubbles)
// - Internal agent IDs are arbitrary.
//
// This implementation of Eq is mostly designed to help fuzz testing. It is not optimized for
// performance.

use rle::{HasLength, SplitableSpan};
use rle::zip::rle_zip;
use crate::{ROOT_AGENT, ROOT_TIME};
use crate::list::frontier::frontier_eq;
use crate::list::{OpLog, Time};
use crate::list::history::MinimalHistoryEntry;
use crate::rle::KVPair;

const VERBOSE: bool = true;

impl PartialEq<Self> for OpLog {
    fn eq(&self, other: &Self) -> bool {
        // This implementation is based on the equivalent version in the original diamond types
        // implementation.

        // Fields to check:
        // - [x] client_with_localtime, client_data,
        // - [x] operations (+ ins_content / del_content)
        // - [x] history
        // - [x] frontier

        if !frontier_eq(&self.frontier, &other.frontier) { return false; }
        assert_eq!(self.len(), other.len(), "Oplog lengths must match if frontiers match");

        // [self.agent] => other.agent.
        // let agent_a_to_b = agent_map_from(self, other);
        let mut agent_a_to_b = Vec::new();
        for c in self.client_data.iter() {
            // If there's no corresponding client in other (and the agent is actually in use), the
            // oplogs don't match.
            let other_agent = if let Some(other_agent) = other.get_agent_id(&c.name) {
                if other.client_data[other_agent as usize].get_next_seq() != c.get_next_seq() {
                    // Make sure we have exactly the same number of edits for each agent.
                    return false;
                }

                other_agent
            } else {
                if c.is_empty() {
                    ROOT_AGENT // Just using this as a placeholder. Could use None but its awkward.
                } else {
                    // Agent missing.
                    if VERBOSE {
                        println!("Oplog does not match because agent ID is missing");
                    }
                    return false;
                }
            };
            agent_a_to_b.push(other_agent);
        }

        let map_time_to_other = |t: Time| -> Time {
            if t == ROOT_TIME { return ROOT_TIME; }
            let mut crdt_id = self.time_to_crdt_id(t);
            crdt_id.agent = agent_a_to_b[crdt_id.agent as usize];
            other.crdt_id_to_time(crdt_id)
        };

        // The core strategy here is we'll iterate through our local operations and make sure they
        // each have a corresponding operation in other. Because self.len == other.len, this will be
        // sufficient.

        // The other approach here would be to go through each agent in self.clients and scan the
        // corresponding changes in other.

        // Note this should be optimized if its going to be used for more than fuzz testing.
        // But this is pretty neat!
        for ((mut op, mut txn), KVPair(_, mut crdt_id)) in rle_zip(
            rle_zip(
                self.iter(),
                self.iter_history()
            ), self.client_with_localtime.iter().cloned()) {

            // println!("op {:?} txn {:?} crdt {:?}", op, txn, crdt_id);

            // Unfortunately the operation range we found might be split up in other. We'll loop
            // grabbing as much of it as we can at a time.
            loop {
                // Look up the corresponding operation in other.

                // This maps via agents - so I think that sort of implicitly checks out.
                let other_time = map_time_to_other(txn.span.start);

                // Lets take a look at the operation.
                let (KVPair(_, other_op_int), offset) = other.operations.find_packed_with_offset(other_time);
                let mut other_op = other_op_int.to_operation(other);
                if offset > 0 { other_op.truncate_keeping_right(offset); }

                if other_op.len() > op.len() {
                    other_op.truncate(op.len());
                }

                let remainder = if op.len() > other_op.len() {
                    Some(op.truncate(other_op.len()))
                } else { None };

                // Whew.
                let len_here = op.len();

                if op != other_op {
                    if VERBOSE { println!("Ops do not match:\n{:?}\n{:?}", op, other_op); }
                    return false;
                }

                // Ok, and we also need to check the txns match.
                let (other_txn_entry, offset) = other.history.entries.find_packed_with_offset(other_time);
                let mut other_txn: MinimalHistoryEntry = other_txn_entry.clone().into();
                if offset > 0 { other_txn.truncate_keeping_right(offset); }
                if other_txn.len() > len_here {
                    other_txn.truncate(len_here);
                }

                // We can't just compare txns because the parents need to be mapped!
                let mapped_start = map_time_to_other(txn.span.start);
                let mut mapped_txn = MinimalHistoryEntry {
                    span: (mapped_start..mapped_start + len_here).into(),
                    parents: txn.parents.iter().map(|t| map_time_to_other(*t)).collect()
                };
                mapped_txn.parents.sort_unstable();

                if other_txn != mapped_txn {
                    if VERBOSE { println!("Txns do not match {:?} (was {:?}) != {:?}", mapped_txn, txn, other_txn); }
                    return false;
                }

                if let Some(rem) = remainder {
                    op = rem;
                } else { break; }
                crdt_id.seq_range.start += len_here;
                txn.truncate_keeping_right(len_here);
            }
        }

        true
    }
}

impl Eq for OpLog {}


#[cfg(test)]
mod test {
    use crate::list::OpLog;
    use crate::ROOT_TIME;

    fn is_eq(a: &OpLog, b: &OpLog) -> bool {
        let a_eq_b = a.eq(b);
        let b_eq_a = b.eq(a);
        if a_eq_b != b_eq_a { dbg!(a_eq_b, b_eq_a); }
        assert_eq!(a_eq_b, b_eq_a);
        a_eq_b
    }

    #[test]
    fn eq_smoke_test() {
        let mut a = OpLog::new();
        a.get_or_create_agent_id("seph");
        a.get_or_create_agent_id("mike");
        a.push_insert_at(0, &[ROOT_TIME], 0, "Aa");
        a.push_insert_at(1, &[ROOT_TIME], 0, "b");
        a.push_delete_at(0, &[1, 2], 0, 2);

        // Same history, different order.
        let mut b = OpLog::new();
        b.get_or_create_agent_id("mike");
        b.get_or_create_agent_id("seph");
        b.push_insert_at(0, &[ROOT_TIME], 0, "b");
        b.push_insert_at(1, &[ROOT_TIME], 0, "Aa");
        b.push_delete_at(1, &[0, 2], 0, 2);

        assert!(is_eq(&a, &b));

        // And now with the edits interleaved
        let mut c = OpLog::new();
        c.get_or_create_agent_id("seph");
        c.get_or_create_agent_id("mike");
        c.push_insert_at(0, &[ROOT_TIME], 0, "A");
        c.push_insert_at(1, &[ROOT_TIME], 0, "b");
        c.push_insert_at(0, &[0], 1, "a");
        c.push_delete_at(0, &[1, 2], 0, 2);

        assert!(is_eq(&a, &c));
        assert!(is_eq(&b, &c));
    }
}