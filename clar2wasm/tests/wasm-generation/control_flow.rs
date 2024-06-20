use clar2wasm::tools::crosscheck;
use clarity::vm::Value;
use proptest::prelude::{prop, proptest};
use proptest::strategy::Strategy;

use crate::PropValue;

fn generate_begin_expressions(
    maximum_expressions: usize,
) -> impl Strategy<Value = (String, PropValue, bool)> {
    prop::collection::vec(PropValue::any(), 1..=maximum_expressions).prop_map(move |values| {
        let mut expressions = String::new();
        let len = values.len();
        let mut is_response_intermediary = false;

        for (i, v) in values.iter().enumerate() {
            if i != len - 1 {
                if let Value::Response(_) = v.0 {
                    is_response_intermediary = true;
                }
            }

            if !expressions.is_empty() {
                expressions.push(' ');
            }

            expressions.push_str(&format!("{v}"));
        }

        let last_value = values.last().unwrap().clone();

        (expressions, last_value, is_response_intermediary)
    })
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn unwrap_panic_optional(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(unwrap-panic (some {val}))"#),
            Ok(Some(val.into()))
        );
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn unwrap_panic_response_ok(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(unwrap-panic (ok {val}))"#),
            Ok(Some(val.into()))
        );
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn unwrap_panic_response_err(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(unwrap-panic (err {val}))"#),
            Err(())
        );
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn unwrap_err_panic(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(unwrap-err-panic (err {val}))"#),
            Ok(Some(val.into()))
        );
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn unwrap_err_panic_ok(val in PropValue::any()) {
        crosscheck(
            &format!(r#"(unwrap-err-panic (ok {val}))"#),
            Err(())
        );
    }
}

proptest! {
    #![proptest_config(super::runtime_config())]

    #[test]
    fn begin((expr, expected_val, is_response_intermediary) in generate_begin_expressions(20)) {
        let expr=format!("(begin {})", expr);

        let expected_val:Result<Option<Value>, ()> = if is_response_intermediary{
            Err(())
        } else{
            Ok(Some(expected_val.into()))
        };

        crosscheck(
            &expr,
            expected_val
        );

    }
}
