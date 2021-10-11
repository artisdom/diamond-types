use diamond_types_positional::list::*;
use crdt_testdata::{TestTxn, TestPatch};
use diamond_types_positional::list::operation::{InsDelTag, PositionalComponent};

pub fn apply_edits(doc: &mut ListCRDT, txns: &Vec<TestTxn>) {
    let id = doc.get_or_create_agent_id("jeremy");

    let mut positional: Vec<PositionalComponent> = Vec::with_capacity(3);
    let mut content = String::new();

    for (_i, txn) in txns.iter().enumerate() {
        for TestPatch(pos, del_span, ins_content) in &txn.patches {
            positional.clear();
            content.clear();

            if *del_span > 0 {
                positional.push(PositionalComponent {
                    pos: *pos,
                    len: *del_span,
                    rev: false,
                    content_known: false,
                    tag: InsDelTag::Del
                });
            }

            if !ins_content.is_empty() {
                positional.push(PositionalComponent {
                    pos: *pos,
                    len: ins_content.chars().count(),
                    rev: false,
                    content_known: true,
                    tag: InsDelTag::Ins
                });
                content.push_str(ins_content.as_str());
            }

            doc.apply_local_operation(id, positional.as_slice(), content.as_str());
        }
    }
}