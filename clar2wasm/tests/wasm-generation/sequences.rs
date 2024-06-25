use clar2wasm::tools::crosscheck;
use clarity::vm::types::{CharType, ListData, ListTypeData, SequenceData, TypeSignature};
use clarity::vm::Value;
use proptest::prelude::*;

use crate::{bool, int, prop_signature, type_string, PropValue, TypePrinter};

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn append_value_to_list(mut values in (prop_signature(), 1usize..16).prop_flat_map(|(ty, size)| PropValue::many_from_type(ty, size))) {
        let expected = Value::cons_list_unsanitized(values.iter().cloned().map(Value::from).collect()).unwrap();

        let elem = values.pop().unwrap();
        let values = PropValue::try_from(values).unwrap();

        crosscheck(&format!("(append {values} {elem})"), Ok(Some(expected)))
    }

    #[test]
    fn double_append_value_to_list(mut values in (prop_signature(), 2usize..16).prop_flat_map(|(ty, size)| PropValue::many_from_type(ty, size))) {
        let expected = Value::cons_list_unsanitized(values.iter().cloned().map(Value::from).collect()).unwrap();

        let elem_last = values.pop().unwrap();
        let elem_before_last = values.pop().unwrap();
        let values = PropValue::try_from(values).unwrap();

        crosscheck(&format!("(append (append {values} {elem_before_last}) {elem_last})"), Ok(Some(expected)))
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn as_max_len_equal_max_len_is_some((max_len, value) in (0usize..=16).prop_ind_flat_map2(PropValue::any_sequence)) {
        crosscheck(
            &format!("(as-max-len? {value} u{max_len})"),
            Ok(Some(Value::some(value.into()).unwrap()))
        )
    }

    #[test]
    fn as_max_len_smaller_than_len_is_none((max_len, value) in (1usize..=16).prop_ind_flat_map2(PropValue::any_sequence)) {
        crosscheck(
            &format!("(as-max-len? {value} u{})", max_len-1),
            Ok(Some(Value::none()))
        )
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn concat_crosscheck((seq1, seq2) in (0usize..=16).prop_flat_map(PropValue::any_sequence).prop_ind_flat_map2(|seq1| PropValue::from_type(TypeSignature::type_of(&seq1.into()).expect("Could not get type signature")))) {
        let snippet = format!("(concat {seq1} {seq2})");

        let expected = {
            let Value::Sequence(mut seq_data1) = seq1.into() else { unreachable!() };
            let Value::Sequence(seq_data2) = seq2.into() else { unreachable!() };
            seq_data1.concat(&clarity::types::StacksEpochId::latest(), seq_data2).expect("Unable to concat sequences");
            Value::Sequence(seq_data1)
        };

        crosscheck(&snippet, Ok(Some(expected)));
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn element_at_crosscheck((seq, idx) in (1usize..=16).prop_flat_map(|max_len| (PropValue::any_sequence(max_len), (0..max_len)))) {
        let snippet = format!("(element-at? {seq} u{idx})");

        let expected = {
            let Value::Sequence(seq_data) = seq.into() else { unreachable!() };
            seq_data.element_at(idx).expect("element_at failed").map_or_else(Value::none, |v| Value::some(v).unwrap())
        };

        crosscheck(&snippet, Ok(Some(expected)));
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn len_crosscheck(seq in (1usize..=16).prop_flat_map(PropValue::any_sequence)) {
        let snippet = format!("(len {seq})");

        let expected = {
            let Value::Sequence(seq_data) = seq.into() else { unreachable!() };
            Value::UInt(seq_data.len() as u128)
        };

        crosscheck(&snippet, Ok(Some(expected)));
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn slice_crosscheck_valid_range(
        (seq, lo, hi) in (1usize..=16)
        .prop_flat_map(PropValue::any_sequence)
        .prop_ind_flat_map2(|seq| 0..extract_sequence(seq).len())
        .prop_ind_flat_map2(|(seq, lo)| lo..extract_sequence(seq).len())
        .prop_map(|((seq, lo), hi)| (seq, lo, hi))
    )
    {
        let snippet = format!("(slice? {seq} u{lo} u{hi})");

        let expected =
            Value::some(
                extract_sequence(seq)
                .slice(&clarity::types::StacksEpochId::latest(), lo, hi)
                .expect("Could not take a slice from sequence")
            ).unwrap();

        crosscheck(&snippet, Ok(Some(expected)));
    }

    #[test]
    fn slice_crosscheck_invalid_range(
        (seq, lo, hi) in (1usize..=16)
        .prop_flat_map(PropValue::any_sequence)
        .prop_ind_flat_map2(|seq| 0..extract_sequence(seq).len())
        .prop_ind_flat_map2(|(seq, lo)| lo..extract_sequence(seq).len())
        .prop_map(|((seq, lo), hi)| (seq, lo, hi))
    )
    {
        // always make sure hi is strictly larger than lo
        let snippet = format!("(slice? {seq} (+ u{hi} u1) u{lo})");
        let expected = Value::none();

        crosscheck(&snippet, Ok(Some(expected)));
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn crosscheck_map_add(
        seq in proptest::collection::vec(proptest::collection::vec(1u128..=1000, 1..=100), 1..=50)
    ) {

        let result: Vec<_> = seq.iter()
        .skip(1).fold(seq[0].clone(), |acc, vecint| {
            acc.into_iter()
            .zip(vecint.iter())
            .map(|(x, y)| x + y)
            .collect()
        })
        .iter().map(|el| Value::UInt(*el)).collect();

        let expected = Value::Sequence(
            SequenceData::List(
                ListData {
                    data: result.clone(),
                    type_signature: ListTypeData::new_list(TypeSignature::UIntType, result.len() as u32).unwrap()
                }
            )
        );

        let lists: Vec<_> = seq.iter().map(|v| {
            v.iter().map(|&el| {
                Value::UInt(el)
            }).collect::<Vec<_>>()
        })
        .map(|v| {
            Value::Sequence(
                SequenceData::List(
                    ListData {
                        data: v.clone(),
                        type_signature: ListTypeData::new_list(TypeSignature::UIntType, v.len() as u32).unwrap()
                    }
                )
            )
        })
        .map(PropValue::from).collect();

        let lists_str: String = lists.iter().map(|el| el.to_string() + " ").collect();
        let snippet = format!("(map + {})", lists_str);

        crosscheck(
            &snippet,
            Ok(Some(expected))
        )
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn crosscheck_fold(
        seq in proptest::collection::vec(1u128..=1000, 1..=100)
    ) {

        let result =
        seq.iter().fold(0, |a, b| a + b);

        let expected = Value::UInt(
            result
        );

        let snippet = format!("(fold + (list {}) u0)", seq.iter().map(|f| format!("u{f} ")).collect::<String>());

        crosscheck(
            &snippet,
            Ok(Some(expected))
        )
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn crosscheck_map_not(
        seq in proptest::collection::vec(bool(), 1..=100)
        .prop_map(|v| {
            Value::Sequence(
                SequenceData::List(
                    ListData {
                        data: v.clone(),
                        type_signature: ListTypeData::new_list(TypeSignature::BoolType, v.len() as u32).unwrap()
                    }
                )
            )
        }).prop_map(PropValue::from)
    ) {
        let expected = extract_sequence(seq.clone());
        let snippet = format!("(map not (map not {seq}))");

        crosscheck(
            &snippet,
            Ok(Some(Value::Sequence(expected)))
        )
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn crosscheck_map_concat_int(
        seq_1 in proptest::collection::vec(int(), 1..=100)
            .prop_map(|v| {
                Value::Sequence(
                    SequenceData::List(
                        ListData {
                            data: v.clone(),
                            type_signature: ListTypeData::new_list(TypeSignature::IntType, v.len() as u32).unwrap()
                        }
                    )
                )
            }).prop_map(PropValue::from),
        seq_2 in proptest::collection::vec(int(), 1..=100)
            .prop_map(|v| {
                Value::Sequence(
                    SequenceData::List(
                        ListData {
                            data: v.clone(),
                            type_signature: ListTypeData::new_list(TypeSignature::IntType, v.len() as u32).unwrap()
                        }
                    )
                )
            }).prop_map(PropValue::from)
    ) {
        let mut expected = extract_sequence(seq_1.clone());
        expected.concat(
            &clarity::types::StacksEpochId::latest(),
            extract_sequence(seq_2.clone())
        ).expect("Could not concat sequences");

        crosscheck(
            &format!(r#"(define-private (fun (a (list 100 int)) (b (list 100 int))) (concat a b)) (try! (element-at (map fun (list {seq_1}) (list {seq_2})) u0))"#),
            Ok(Some(Value::Sequence(expected)))
        )
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn crosscheck_replace_at(
        (seq, source, dest) in (1usize..=20).prop_flat_map(|seq_size| {
            (PropValue::any_sequence(seq_size),
            // ranges from 0 to sequence_size - 1
            // to not occur on operations out of boundaries.
            (0usize..=seq_size - 1),
            (0usize..=seq_size - 1))
        }).no_shrink()
    ) {
        let list_ty = seq.type_string();

        let Value::Sequence(seq_data) = seq.clone().into() else { unreachable!() };

        let repl_ty = match &seq_data {
            SequenceData::Buffer(_) => "(buff 1)".to_owned(),
            SequenceData::String(CharType::ASCII(_)) => "(string-ascii 1)".to_owned(),
            SequenceData::String(CharType::UTF8(_)) => "(string-utf8 1)".to_owned(),
            SequenceData::List(ld) => type_string(ld.type_signature.get_list_item_type()),
        };

        let (expected, el) = {
            // collect an element from the sequence at 'source' position.
            let el = seq_data.clone().element_at(source).expect("element_at failed").map_or_else(Value::none, |value| value);
            // replace the element at 'dest' position
            // with the collected element from the 'source' position.
            (seq_data.replace_at(
                &clarity::types::StacksEpochId::latest(),
                dest,
                el.clone()
            ).expect("replace_at failed"),
            PropValue::from(el)) // returning that to be used by the 'replace-at' Clarity function.
        };

        // Workaround needed for https://github.com/stacks-network/stacks-core/issues/4622
        let snippet = format!(r#"
            (define-private (replace-at-workaround? (seq {list_ty}) (idx uint) (repl {repl_ty}))
                (replace-at? seq idx repl)
            )
            (replace-at-workaround? {seq} u{dest} {el})
        "#);

        crosscheck(
            &snippet,
            Ok(Some(expected))
        )
    }
}

fn extract_sequence(sequence: PropValue) -> SequenceData {
    match Value::from(sequence) {
        Value::Sequence(seq_data) => seq_data,
        _ => panic!("Should only call this function on the result of PropValue::any_sequence"),
    }
}
