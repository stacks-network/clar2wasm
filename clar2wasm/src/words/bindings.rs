use clarity::vm::{ClarityName, SymbolicExpression};

use crate::wasm_generator::{ArgumentsExt, GeneratorError, WasmGenerator};
use crate::words::ComplexWord;

#[derive(Debug)]
pub struct Let;

impl ComplexWord for Let {
    fn name(&self) -> ClarityName {
        "let".into()
    }

    fn traverse(
        &self,
        generator: &mut WasmGenerator,
        builder: &mut walrus::InstrSeqBuilder,
        _expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        if args.len() < 2 {
            return Err(GeneratorError::ArgumentLengthError(format!(
                "let expected at least 2 arguments, got {}",
                args.len()
            )));
        };

        let bindings = args.get_list(0)?;

        // Save the current named locals
        let saved_locals = generator.bindings.clone();

        // Traverse the bindings
        for i in 0..bindings.len() {
            let pair = bindings.get_list(i)?;
            let name = pair.get_name(0)?;
            let value = pair.get_expr(1)?;
            // make sure name does not collide with builtin symbols
            if generator.is_reserved_name(name) {
                return Err(GeneratorError::InternalError(format!(
                    "Name already used {:?}",
                    name
                )));
            }

            // Traverse the value
            generator.traverse_expr(builder, value)?;

            // Store store the value in locals, and save to the var map
            let ty = generator
                .get_expr_type(value)
                .ok_or_else(|| {
                    GeneratorError::TypeError("let value expression must be typed".to_owned())
                })?
                .clone();
            let locals = generator.save_to_locals(builder, &ty, true);

            // Add these named locals to the map
            generator.bindings.insert(name.clone(), ty, locals);
        }

        // WORKAROUND: need to set the last statement type to the type of the let expression
        let expr_ty = generator
            .get_expr_type(_expr)
            .ok_or_else(|| GeneratorError::TypeError("let expression should be typed".to_owned()))?
            .clone();
        generator.set_expr_type(
            args.last().ok_or_else(|| {
                GeneratorError::TypeError(
                    "let expression should have at least one statement".to_owned(),
                )
            })?,
            expr_ty,
        )?;

        // Traverse the body
        generator.traverse_statement_list(builder, &args[1..])?;

        // Restore the named locals
        generator.bindings = saved_locals;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use clarity::vm::Value;

    use crate::tools::{crosscheck, crosscheck_compare_only, crosscheck_expect_failure};

    #[cfg(feature = "test-clarity-v1")]
    mod clarity_v1 {
        use clarity::types::StacksEpochId;

        use super::*;
        use crate::tools::crosscheck_with_epoch;

        #[test]
        fn validate_let_epoch() {
            // Epoch20
            crosscheck_with_epoch(
                "(let ((index-of? 2)) (+ index-of? index-of?))",
                Ok(Some(Value::Int(4))),
                StacksEpochId::Epoch20,
            );
        }
    }

    #[test]
    fn let_less_than_two_args() {
        crosscheck_expect_failure("(let ((current-count (count u1))))");
    }

    #[test]
    fn clar_let_disallow_builtin_names() {
        // It's not allowed to use names of user-defined functions as bindings
        const ERR: &str = "
(define-private (test)
 (let ((+ u3))
   +))";

        crosscheck_expect_failure(&format!("{ERR} (test)"));
    }

    #[test]
    fn clar_let_disallow_user_defined_names() {
        // It's not allowed to use names of user-defined functions as bindings
        const ERR: &str = "
(define-private (test)
 (let ((test u3))
    test))";

        crosscheck_expect_failure(&format!("{ERR} (test)"));
    }

    #[test]
    fn let_with_multiple_statements() {
        crosscheck_compare_only(
            r#"
                (define-data-var count uint u0)

                (define-public (decrement)
                    (let ((current-count (var-get count)))
                        (asserts! (> current-count u0) (err u1))
                        (ok (var-set count (- current-count u1)))
                    )
                )
            "#,
        )
    }

    #[test]
    fn validate_let() {
        // Reserved keyword
        crosscheck_expect_failure("(let ((map 2)) (+ map map))");

        // Custom variable name
        crosscheck("(let ((a 2)) (+ a a))", Ok(Some(Value::Int(4))));

        // Custom variable name duplicate
        crosscheck_expect_failure("(let ((a 2) (a 3)) (+ a a))");
    }
}
