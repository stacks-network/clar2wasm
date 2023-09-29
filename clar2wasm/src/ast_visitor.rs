// This file is copied from [Clarinet](https://github.com/hirosystems/clarinet),
// which is licensed under the GPPLv3 license.

use clarity::vm::functions::define::DefineFunctions;
use clarity::vm::functions::NativeFunctions;
use clarity::vm::representations::SymbolicExpressionType::*;
use clarity::vm::representations::{Span, TraitDefinition};
use clarity::vm::types::{PrincipalData, QualifiedContractIdentifier, TraitIdentifier, Value};
use clarity::vm::{ClarityName, ClarityVersion, SymbolicExpression, SymbolicExpressionType};
use std::collections::HashMap;
use walrus::InstrSeqBuilder;

#[derive(Clone)]
pub struct TypedVar<'c> {
    pub name: &'c ClarityName,
    pub type_expr: &'c SymbolicExpression,
    pub decl_span: Span,
}

lazy_static! {
    // Since the AST Visitor may be used before other checks have been performed,
    // we may need a default value for some expressions. This can be used for a
    // missing `ClarityName`.
    static ref DEFAULT_NAME: ClarityName = ClarityName::from("placeholder__");
    static ref DEFAULT_EXPR: SymbolicExpression = SymbolicExpression::atom(DEFAULT_NAME.clone());
}

/// The ASTVisitor trait specifies the interfaces needed to build a visitor
/// to walk a Clarity abstract syntax tree (AST). All methods have default
/// implementations so that any left undefined in an implementation will
/// perform a standard walk through the AST, ensuring that all sub-expressions
/// are visited as appropriate. If a `traverse_*` method is implemented, then
/// the implementation is responsible for traversing the sub-expressions.
///
/// Traversal is post-order, so the sub-expressions are visited before the
/// parent is visited. To walk through an example, if we visit the AST for the
/// Clarity expression `(+ a 1)`, we would hit the following methods in order:
/// 1. `traverse_expr`: `(+ a 1)`
/// 2. `traverse_list`: `(+ a 1)`
/// 3. `traverse_arithmetic`: `(+ a 1)`
/// 4. `traverse_expr`: `a`
/// 5. `visit_atom`: `a`
/// 6. `traverse_expr`: `1`
/// 7. `visit_literal_value`: `1`
/// 8. `visit_arithmetic`: `(+ a 1)`
///
/// When implementing the `ASTVisitor` trait, the default `traverse_*` methods
/// should be used when possible, implementing only the `visit_*` methods.
/// `traverse_*` methods should only be overridden when some action must be
/// taken before the sub-expressions are visited.
pub trait ASTVisitor {
    fn traverse_expr<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        match &expr.expr {
            AtomValue(value) => self.visit_atom_value(builder, expr, value),
            Atom(name) => self.visit_atom(builder, expr, name),
            List(exprs) => self.traverse_list(builder, expr, exprs),
            LiteralValue(value) => self.visit_literal_value(builder, expr, value),
            Field(field) => self.visit_field(builder, expr, field),
            TraitReference(name, trait_def) => {
                self.visit_trait_reference(builder, expr, name, trait_def)
            }
        }
    }

    // AST level traverse/visit methods

    fn traverse_list<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        list: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if let Some((function_name, args)) = list.split_first() {
            if let Some(function_name) = function_name.match_atom() {
                if let Some(define_function) = DefineFunctions::lookup_by_name(function_name) {
                    builder = match define_function {
                        DefineFunctions::Constant => self.traverse_define_constant(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        DefineFunctions::PrivateFunction
                        | DefineFunctions::ReadOnlyFunction
                        | DefineFunctions::PublicFunction => {
                            match args.get(0).unwrap_or(&DEFAULT_EXPR).match_list() {
                                Some(signature) => {
                                    let name = signature
                                        .get(0)
                                        .and_then(|n| n.match_atom())
                                        .unwrap_or(&DEFAULT_NAME);
                                    let params = match signature.len() {
                                        0 | 1 => None,
                                        _ => match_pairs_list(&signature[1..]),
                                    };
                                    let body = args.get(1).unwrap_or(&DEFAULT_EXPR);

                                    match define_function {
                                        DefineFunctions::PrivateFunction => self
                                            .traverse_define_private(
                                                builder, expr, name, params, body,
                                            ),
                                        DefineFunctions::ReadOnlyFunction => self
                                            .traverse_define_read_only(
                                                builder, expr, name, params, body,
                                            ),
                                        DefineFunctions::PublicFunction => self
                                            .traverse_define_public(
                                                builder, expr, name, params, body,
                                            ),
                                        _ => unreachable!(),
                                    }
                                }
                                _ => Err(builder),
                            }
                        }
                        DefineFunctions::NonFungibleToken => self.traverse_define_nft(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        DefineFunctions::FungibleToken => self.traverse_define_ft(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1),
                        ),
                        DefineFunctions::Map => self.traverse_define_map(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        DefineFunctions::PersistedVariable => self.traverse_define_data_var(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        DefineFunctions::Trait => {
                            let params = if !args.is_empty() { &args[1..] } else { &[] };
                            self.traverse_define_trait(
                                builder,
                                expr,
                                args.get(0)
                                    .unwrap_or(&DEFAULT_EXPR)
                                    .match_atom()
                                    .unwrap_or(&DEFAULT_NAME),
                                params,
                            )
                        }
                        DefineFunctions::UseTrait => self.traverse_use_trait(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_field()
                                .unwrap_or(&TraitIdentifier {
                                    contract_identifier: QualifiedContractIdentifier::transient(),
                                    name: DEFAULT_NAME.clone(),
                                }),
                        ),
                        DefineFunctions::ImplTrait => self.traverse_impl_trait(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_field()
                                .unwrap_or(&TraitIdentifier {
                                    contract_identifier: QualifiedContractIdentifier::transient(),
                                    name: DEFAULT_NAME.clone(),
                                }),
                        ),
                    }?;
                } else if let Some(native_function) = NativeFunctions::lookup_by_name_at_version(
                    function_name,
                    &ClarityVersion::latest(), // FIXME(brice): this should probably be passed in
                ) {
                    use clarity::vm::functions::NativeFunctions::*;
                    builder = match native_function {
                        Add | Subtract | Multiply | Divide | Modulo | Power | Sqrti | Log2 => {
                            self.traverse_arithmetic(builder, expr, native_function, args)
                        }
                        BitwiseXor => self.traverse_binary_bitwise(
                            builder,
                            expr,
                            native_function,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        CmpLess | CmpLeq | CmpGreater | CmpGeq | Equals => {
                            self.traverse_comparison(builder, expr, native_function, args)
                        }
                        And | Or => {
                            self.traverse_lazy_logical(builder, expr, native_function, args)
                        }
                        Not => self.traverse_logical(builder, expr, native_function, args),
                        ToInt | ToUInt => self.traverse_int_cast(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        If => self.traverse_if(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Let => {
                            let bindings = match_pairs(args.get(0).unwrap_or(&DEFAULT_EXPR))
                                .unwrap_or_default();
                            let params = if !args.is_empty() { &args[1..] } else { &[] };
                            self.traverse_let(builder, expr, &bindings, params)
                        }
                        ElementAt | ElementAtAlias => self.traverse_element_at(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        IndexOf | IndexOfAlias => self.traverse_index_of(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Map => {
                            let name = args
                                .get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME);
                            let params = if !args.is_empty() { &args[1..] } else { &[] };
                            self.traverse_map(builder, expr, name, params)
                        }
                        Fold => {
                            let name = args
                                .get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME);
                            self.traverse_fold(
                                builder,
                                expr,
                                name,
                                args.get(1).unwrap_or(&DEFAULT_EXPR),
                                args.get(2).unwrap_or(&DEFAULT_EXPR),
                            )
                        }
                        Append => self.traverse_append(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Concat => self.traverse_concat(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        AsMaxLen => {
                            match args.get(1).unwrap_or(&DEFAULT_EXPR).match_literal_value() {
                                Some(Value::UInt(length)) => self.traverse_as_max_len(
                                    builder,
                                    expr,
                                    args.get(0).unwrap_or(&DEFAULT_EXPR),
                                    *length,
                                ),
                                _ => Err(builder),
                            }
                        }
                        Len => {
                            self.traverse_len(builder, expr, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        ListCons => self.traverse_list_cons(builder, expr, args),
                        FetchVar => self.traverse_var_get(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                        ),
                        SetVar => self.traverse_var_set(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        FetchEntry => {
                            let name = args
                                .get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME);
                            self.traverse_map_get(
                                builder,
                                expr,
                                name,
                                args.get(1).unwrap_or(&DEFAULT_EXPR),
                            )
                        }
                        SetEntry => {
                            let name = args
                                .get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME);
                            self.traverse_map_set(
                                builder,
                                expr,
                                name,
                                args.get(1).unwrap_or(&DEFAULT_EXPR),
                                args.get(2).unwrap_or(&DEFAULT_EXPR),
                            )
                        }
                        InsertEntry => {
                            let name = args
                                .get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME);
                            self.traverse_map_insert(
                                builder,
                                expr,
                                name,
                                args.get(1).unwrap_or(&DEFAULT_EXPR),
                                args.get(2).unwrap_or(&DEFAULT_EXPR),
                            )
                        }
                        DeleteEntry => {
                            let name = args
                                .get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME);
                            self.traverse_map_delete(
                                builder,
                                expr,
                                name,
                                args.get(1).unwrap_or(&DEFAULT_EXPR),
                            )
                        }
                        TupleCons => self.traverse_tuple(
                            builder,
                            expr,
                            &match_tuple(expr).unwrap_or_default(),
                        ),
                        TupleGet => self.traverse_get(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        TupleMerge => self.traverse_merge(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Begin => self.traverse_begin(builder, expr, args),
                        Hash160 | Sha256 | Sha512 | Sha512Trunc256 | Keccak256 => self
                            .traverse_hash(
                                builder,
                                expr,
                                native_function,
                                args.get(0).unwrap_or(&DEFAULT_EXPR),
                            ),
                        Secp256k1Recover => self.traverse_secp256k1_recover(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Secp256k1Verify => self.traverse_secp256k1_verify(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Print => {
                            self.traverse_print(builder, expr, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        ContractCall => {
                            let function_name = args
                                .get(1)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME);
                            let params = if args.len() >= 2 { &args[2..] } else { &[] };
                            if let SymbolicExpressionType::LiteralValue(Value::Principal(
                                PrincipalData::Contract(ref contract_identifier),
                            )) = args.get(0).unwrap_or(&DEFAULT_EXPR).expr
                            {
                                self.traverse_static_contract_call(
                                    builder,
                                    expr,
                                    contract_identifier,
                                    function_name,
                                    params,
                                )
                            } else {
                                self.traverse_dynamic_contract_call(
                                    builder,
                                    expr,
                                    args.get(0).unwrap_or(&DEFAULT_EXPR),
                                    function_name,
                                    params,
                                )
                            }
                        }
                        AsContract => self.traverse_as_contract(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        ContractOf => self.traverse_contract_of(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        PrincipalOf => self.traverse_principal_of(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        AtBlock => self.traverse_at_block(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        GetBlockInfo => self.traverse_get_block_info(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        ConsError => {
                            self.traverse_err(builder, expr, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        ConsOkay => {
                            self.traverse_ok(builder, expr, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        ConsSome => {
                            self.traverse_some(builder, expr, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        DefaultTo => self.traverse_default_to(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Asserts => self.traverse_asserts(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        UnwrapRet => self.traverse_unwrap(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Unwrap => self.traverse_unwrap_panic(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        IsOkay => {
                            self.traverse_is_ok(builder, expr, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        IsNone => self.traverse_is_none(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        IsErr => self.traverse_is_err(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        IsSome => self.traverse_is_some(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Filter => self.traverse_filter(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        UnwrapErrRet => self.traverse_unwrap_err(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        UnwrapErr => self.traverse_unwrap_err(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Match => {
                            if args.len() == 4 {
                                self.traverse_match_option(
                                    builder,
                                    expr,
                                    args.get(0).unwrap_or(&DEFAULT_EXPR),
                                    args.get(1)
                                        .unwrap_or(&DEFAULT_EXPR)
                                        .match_atom()
                                        .unwrap_or(&DEFAULT_NAME),
                                    args.get(2).unwrap_or(&DEFAULT_EXPR),
                                    args.get(3).unwrap_or(&DEFAULT_EXPR),
                                )
                            } else {
                                self.traverse_match_response(
                                    builder,
                                    expr,
                                    args.get(0).unwrap_or(&DEFAULT_EXPR),
                                    args.get(1)
                                        .unwrap_or(&DEFAULT_EXPR)
                                        .match_atom()
                                        .unwrap_or(&DEFAULT_NAME),
                                    args.get(2).unwrap_or(&DEFAULT_EXPR),
                                    args.get(3)
                                        .unwrap_or(&DEFAULT_EXPR)
                                        .match_atom()
                                        .unwrap_or(&DEFAULT_NAME),
                                    args.get(4).unwrap_or(&DEFAULT_EXPR),
                                )
                            }
                        }
                        TryRet => {
                            self.traverse_try(builder, expr, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        StxBurn => self.traverse_stx_burn(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        StxTransfer | StxTransferMemo => self.traverse_stx_transfer(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                            args.get(3),
                        ),
                        GetStxBalance => self.traverse_stx_get_balance(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        BurnToken => self.traverse_ft_burn(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        TransferToken => self.traverse_ft_transfer(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                            args.get(3).unwrap_or(&DEFAULT_EXPR),
                        ),
                        GetTokenBalance => self.traverse_ft_get_balance(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        GetTokenSupply => self.traverse_ft_get_supply(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                        ),
                        MintToken => self.traverse_ft_mint(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        BurnAsset => self.traverse_nft_burn(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        TransferAsset => self.traverse_nft_transfer(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                            args.get(3).unwrap_or(&DEFAULT_EXPR),
                        ),
                        MintAsset => self.traverse_nft_mint(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        GetAssetOwner => self.traverse_nft_get_owner(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        BuffToIntLe | BuffToUIntLe | BuffToIntBe | BuffToUIntBe => self
                            .traverse_buff_cast(
                                builder,
                                expr,
                                args.get(0).unwrap_or(&DEFAULT_EXPR),
                            ),
                        IsStandard => self.traverse_is_standard(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        PrincipalDestruct => self.traverse_principal_destruct(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        PrincipalConstruct => self.traverse_principal_construct(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2),
                        ),
                        StringToInt | StringToUInt => self.traverse_string_to_int(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        IntToAscii | IntToUtf8 => self.traverse_int_to_string(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        GetBurnBlockInfo => self.traverse_get_burn_block_info(
                            builder,
                            expr,
                            args.get(0)
                                .unwrap_or(&DEFAULT_EXPR)
                                .match_atom()
                                .unwrap_or(&DEFAULT_NAME),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        StxGetAccount => self.traverse_stx_get_account(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        Slice => self.traverse_slice(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        ToConsensusBuff => self.traverse_to_consensus_buff(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                        ),
                        FromConsensusBuff => self.traverse_from_consensus_buff(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                        ReplaceAt => self.traverse_replace_at(
                            builder,
                            expr,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                            args.get(2).unwrap_or(&DEFAULT_EXPR),
                        ),
                        BitwiseAnd | BitwiseOr | BitwiseXor2 => {
                            self.traverse_bitwise(builder, expr, native_function, args)
                        }
                        BitwiseNot => {
                            self.traverse_bitwise_not(builder, args.get(0).unwrap_or(&DEFAULT_EXPR))
                        }
                        BitwiseLShift | BitwiseRShift => self.traverse_bit_shift(
                            builder,
                            expr,
                            native_function,
                            args.get(0).unwrap_or(&DEFAULT_EXPR),
                            args.get(1).unwrap_or(&DEFAULT_EXPR),
                        ),
                    }?;
                } else {
                    builder =
                        self.traverse_call_user_defined(builder, expr, function_name, args)?;
                }
            }
        }
        self.visit_list(builder, expr, list)
    }

    fn visit_list<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _list: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn visit_atom_value<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &Value,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn visit_atom<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _atom: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn visit_literal_value<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &Value,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn visit_field<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _field: &TraitIdentifier,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn visit_trait_reference<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _trait_def: &TraitDefinition,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    // Higher level traverse/visit methods

    fn traverse_define_constant<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_define_constant(builder, expr, name, value)
    }

    fn visit_define_constant<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_private<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        parameters: Option<Vec<TypedVar<'_>>>,
        body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, body)?;
        self.visit_define_private(builder, expr, name, parameters, body)
    }

    fn visit_define_private<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _parameters: Option<Vec<TypedVar<'_>>>,
        _body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_read_only<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        parameters: Option<Vec<TypedVar<'_>>>,
        body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, body)?;
        self.visit_define_read_only(builder, expr, name, parameters, body)
    }

    fn visit_define_read_only<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _parameters: Option<Vec<TypedVar<'_>>>,
        _body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_public<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        parameters: Option<Vec<TypedVar<'_>>>,
        body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, body)?;
        self.visit_define_public(builder, expr, name, parameters, body)
    }

    fn visit_define_public<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _parameters: Option<Vec<TypedVar<'_>>>,
        _body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_nft<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        nft_type: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.visit_define_nft(builder, expr, name, nft_type)
    }

    fn visit_define_nft<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _nft_type: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_ft<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        supply: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if let Some(supply_expr) = supply {
            builder = self.traverse_expr(builder, supply_expr)?;
        }

        self.visit_define_ft(builder, expr, name, supply)
    }

    fn visit_define_ft<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _supply: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_map<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        key_type: &SymbolicExpression,
        value_type: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.visit_define_map(builder, expr, name, key_type, value_type)
    }

    fn visit_define_map<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _key_type: &SymbolicExpression,
        _value_type: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_data_var<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        data_type: &SymbolicExpression,
        initial: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, initial)?;
        self.visit_define_data_var(builder, expr, name, data_type, initial)
    }

    fn visit_define_data_var<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _data_type: &SymbolicExpression,
        _initial: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_define_trait<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        functions: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.visit_define_trait(builder, expr, name, functions)
    }

    fn visit_define_trait<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _functions: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_use_trait<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        trait_identifier: &TraitIdentifier,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.visit_use_trait(builder, expr, name, trait_identifier)
    }

    fn visit_use_trait<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _trait_identifier: &TraitIdentifier,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_impl_trait<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        trait_identifier: &TraitIdentifier,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.visit_impl_trait(builder, expr, trait_identifier)
    }

    fn visit_impl_trait<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _trait_identifier: &TraitIdentifier,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_arithmetic<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for operand in operands {
            builder = self.traverse_expr(builder, operand)?;
        }
        self.visit_arithmetic(builder, expr, func, operands)
    }

    fn visit_arithmetic<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: NativeFunctions,
        _operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_binary_bitwise<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: NativeFunctions,
        lhs: &SymbolicExpression,
        rhs: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for operand in &[lhs, rhs] {
            builder = self.traverse_expr(builder, operand)?;
        }
        self.visit_binary_bitwise(builder, expr, func, lhs, rhs)
    }

    fn visit_binary_bitwise<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: NativeFunctions,
        _lhs: &SymbolicExpression,
        _rhs: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_comparison<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for operand in operands {
            builder = self.traverse_expr(builder, operand)?;
        }
        self.visit_comparison(builder, expr, func, operands)
    }

    fn visit_comparison<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: NativeFunctions,
        _operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_lazy_logical<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        function: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for operand in operands {
            builder = self.traverse_expr(builder, operand)?;
        }
        self.visit_lazy_logical(builder, expr, function, operands)
    }

    fn visit_lazy_logical<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _function: NativeFunctions,
        _operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_logical<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        function: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for operand in operands {
            builder = self.traverse_expr(builder, operand)?;
        }
        self.visit_logical(builder, expr, function, operands)
    }

    fn visit_logical<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _function: NativeFunctions,
        _operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_int_cast<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_int_cast(builder, expr, input)
    }

    fn visit_int_cast<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_if<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        cond: &SymbolicExpression,
        then_expr: &SymbolicExpression,
        else_expr: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for &expr in &[cond, then_expr, else_expr] {
            builder = self.traverse_expr(builder, expr)?;
        }
        self.visit_if(builder, expr, cond, then_expr, else_expr)
    }

    fn visit_if<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _cond: &SymbolicExpression,
        _then_expr: &SymbolicExpression,
        _else_expr: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_var_get<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.visit_var_get(builder, expr, name)
    }

    fn visit_var_get<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_var_set<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_var_set(builder, expr, name, value)
    }

    fn visit_var_set<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_map_get<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, key)?;
        self.visit_map_get(builder, expr, name, key)
    }

    fn visit_map_get<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_map_set<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, key)?;
        builder = self.traverse_expr(builder, value)?;
        self.visit_map_set(builder, expr, name, key, value)
    }

    fn visit_map_set<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _key: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_map_insert<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, key)?;
        builder = self.traverse_expr(builder, value)?;
        self.visit_map_insert(builder, expr, name, key, value)
    }

    fn visit_map_insert<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _key: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_map_delete<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, key)?;
        self.visit_map_delete(builder, expr, name, key)
    }

    fn visit_map_delete<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_tuple<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        values: &HashMap<Option<&ClarityName>, &SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for val in values.values() {
            builder = self.traverse_expr(builder, val)?;
        }
        self.visit_tuple(builder, expr, values)
    }

    fn visit_tuple<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _values: &HashMap<Option<&ClarityName>, &SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_get<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        key: &ClarityName,
        tuple: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, tuple)?;
        self.visit_get(builder, expr, key, tuple)
    }

    fn visit_get<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _key: &ClarityName,
        _tuple: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_merge<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        tuple1: &SymbolicExpression,
        tuple2: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, tuple1)?;
        builder = self.traverse_expr(builder, tuple2)?;
        self.visit_merge(builder, expr, tuple1, tuple2)
    }

    fn visit_merge<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _tuple1: &SymbolicExpression,
        _tuple2: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_begin<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        statements: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for stmt in statements {
            builder = self.traverse_expr(builder, stmt)?;
        }
        self.visit_begin(builder, expr, statements)
    }

    fn visit_begin<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _statements: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_hash<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: NativeFunctions,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_hash(builder, expr, func, value)
    }

    fn visit_hash<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: NativeFunctions,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_secp256k1_recover<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        hash: &SymbolicExpression,
        signature: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, hash)?;
        builder = self.traverse_expr(builder, signature)?;
        self.visit_secp256k1_recover(builder, expr, hash, signature)
    }

    fn visit_secp256k1_recover<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _hash: &SymbolicExpression,
        _signature: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_secp256k1_verify<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        hash: &SymbolicExpression,
        signature: &SymbolicExpression,
        public_key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, hash)?;
        builder = self.traverse_expr(builder, signature)?;
        self.visit_secp256k1_verify(builder, expr, hash, signature, public_key)
    }

    fn visit_secp256k1_verify<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _hash: &SymbolicExpression,
        _signature: &SymbolicExpression,
        _public_key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_print<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_print(builder, expr, value)
    }

    fn visit_print<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_static_contract_call<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        contract_identifier: &QualifiedContractIdentifier,
        function_name: &ClarityName,
        args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for arg in args.iter() {
            builder = self.traverse_expr(builder, arg)?;
        }
        self.visit_static_contract_call(builder, expr, contract_identifier, function_name, args)
    }

    fn visit_static_contract_call<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _contract_identifier: &QualifiedContractIdentifier,
        _function_name: &ClarityName,
        _args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_dynamic_contract_call<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        trait_ref: &SymbolicExpression,
        function_name: &ClarityName,
        args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, trait_ref)?;
        for arg in args.iter() {
            builder = self.traverse_expr(builder, arg)?;
        }
        self.visit_dynamic_contract_call(builder, expr, trait_ref, function_name, args)
    }

    fn visit_dynamic_contract_call<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _trait_ref: &SymbolicExpression,
        _function_name: &ClarityName,
        _args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_as_contract<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        inner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, inner)?;
        self.visit_as_contract(builder, expr, inner)
    }

    fn visit_as_contract<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _inner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_contract_of<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, name)?;
        self.visit_contract_of(builder, expr, name)
    }

    fn visit_contract_of<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_principal_of<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        public_key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, public_key)?;
        self.visit_principal_of(builder, expr, public_key)
    }

    fn visit_principal_of<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _public_key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_at_block<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        block: &SymbolicExpression,
        inner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, block)?;
        builder = self.traverse_expr(builder, inner)?;
        self.visit_at_block(builder, expr, block, inner)
    }

    fn visit_at_block<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _block: &SymbolicExpression,
        _inner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_get_block_info<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        prop_name: &ClarityName,
        block: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, block)?;
        self.visit_get_block_info(builder, expr, prop_name, block)
    }

    fn visit_get_block_info<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _prop_name: &ClarityName,
        _block: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_err<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_err(builder, expr, value)
    }

    fn visit_err<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_ok<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_ok(builder, expr, value)
    }

    fn visit_ok<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_some<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_some(builder, expr, value)
    }

    fn visit_some<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_default_to<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        default: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, default)?;
        builder = self.traverse_expr(builder, value)?;
        self.visit_default_to(builder, expr, default, value)
    }

    fn visit_default_to<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _default: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_unwrap<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
        throws: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        builder = self.traverse_expr(builder, throws)?;
        self.visit_unwrap(builder, expr, input, throws)
    }

    fn visit_unwrap<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
        _throws: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_unwrap_err<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
        throws: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        builder = self.traverse_expr(builder, throws)?;
        self.visit_unwrap_err(builder, expr, input, throws)
    }

    fn visit_unwrap_err<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
        _throws: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_is_ok<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_is_ok(builder, expr, value)
    }

    fn visit_is_ok<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_is_none<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_is_none(builder, expr, value)
    }

    fn visit_is_none<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_is_err<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_is_err(builder, expr, value)
    }

    fn visit_is_err<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_is_some<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_is_some(builder, expr, value)
    }

    fn visit_is_some<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_filter<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: &ClarityName,
        sequence: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, sequence)?;
        self.visit_filter(builder, expr, func, sequence)
    }

    fn visit_filter<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: &ClarityName,
        _sequence: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_unwrap_panic<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_unwrap_panic(builder, expr, input)
    }

    fn visit_unwrap_panic<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_unwrap_err_panic<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_unwrap_err_panic(builder, expr, input)
    }

    fn visit_unwrap_err_panic<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_match_option<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
        some_name: &ClarityName,
        some_branch: &SymbolicExpression,
        none_branch: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        builder = self.traverse_expr(builder, some_branch)?;
        builder = self.traverse_expr(builder, none_branch)?;
        self.visit_match_option(builder, expr, input, some_name, some_branch, none_branch)
    }

    fn visit_match_option<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
        _some_name: &ClarityName,
        _some_branch: &SymbolicExpression,
        _none_branch: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    #[allow(clippy::too_many_arguments)]
    fn traverse_match_response<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
        ok_name: &ClarityName,
        ok_branch: &SymbolicExpression,
        err_name: &ClarityName,
        err_branch: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        builder = self.traverse_expr(builder, ok_branch)?;
        builder = self.traverse_expr(builder, err_branch)?;
        self.visit_match_response(
            builder, expr, input, ok_name, ok_branch, err_name, err_branch,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn visit_match_response<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
        _ok_name: &ClarityName,
        _ok_branch: &SymbolicExpression,
        _err_name: &ClarityName,
        _err_branch: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_try<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_try(builder, expr, input)
    }

    fn visit_try<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_asserts<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        cond: &SymbolicExpression,
        thrown: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, cond)?;
        builder = self.traverse_expr(builder, thrown)?;
        self.visit_asserts(builder, expr, cond, thrown)
    }

    fn visit_asserts<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _cond: &SymbolicExpression,
        _thrown: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_stx_burn<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        amount: &SymbolicExpression,
        sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, sender)?;
        self.visit_stx_burn(builder, expr, amount, sender)
    }

    fn visit_stx_burn<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _amount: &SymbolicExpression,
        _sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_stx_transfer<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        amount: &SymbolicExpression,
        sender: &SymbolicExpression,
        recipient: &SymbolicExpression,
        memo: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, sender)?;
        builder = self.traverse_expr(builder, recipient)?;
        if let Some(memo) = memo {
            builder = self.traverse_expr(builder, memo)?;
        }
        self.visit_stx_transfer(builder, expr, amount, sender, recipient, memo)
    }

    fn visit_stx_transfer<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _amount: &SymbolicExpression,
        _sender: &SymbolicExpression,
        _recipient: &SymbolicExpression,
        _memo: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_stx_get_balance<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, owner)?;
        self.visit_stx_get_balance(builder, expr, owner)
    }

    fn visit_stx_get_balance<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_ft_burn<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        amount: &SymbolicExpression,
        sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, sender)?;
        self.visit_ft_burn(builder, expr, token, amount, sender)
    }

    fn visit_ft_burn<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _amount: &SymbolicExpression,
        _sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_ft_transfer<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        amount: &SymbolicExpression,
        sender: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, sender)?;
        builder = self.traverse_expr(builder, recipient)?;
        self.visit_ft_transfer(builder, expr, token, amount, sender, recipient)
    }

    fn visit_ft_transfer<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _amount: &SymbolicExpression,
        _sender: &SymbolicExpression,
        _recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_ft_get_balance<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, owner)?;
        self.visit_ft_get_balance(builder, expr, token, owner)
    }

    fn visit_ft_get_balance<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_ft_get_supply<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.visit_ft_get_supply(builder, expr, token)
    }

    fn visit_ft_get_supply<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_ft_mint<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        amount: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, recipient)?;
        self.visit_ft_mint(builder, expr, token, amount, recipient)
    }

    fn visit_ft_mint<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _amount: &SymbolicExpression,
        _recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_nft_burn<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
        sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, identifier)?;
        builder = self.traverse_expr(builder, sender)?;
        self.visit_nft_burn(builder, expr, token, identifier, sender)
    }

    fn visit_nft_burn<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _identifier: &SymbolicExpression,
        _sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_nft_transfer<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
        sender: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, identifier)?;
        builder = self.traverse_expr(builder, sender)?;
        builder = self.traverse_expr(builder, recipient)?;
        self.visit_nft_transfer(builder, expr, token, identifier, sender, recipient)
    }

    fn visit_nft_transfer<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _identifier: &SymbolicExpression,
        _sender: &SymbolicExpression,
        _recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_nft_mint<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, identifier)?;
        builder = self.traverse_expr(builder, recipient)?;
        self.visit_nft_mint(builder, expr, token, identifier, recipient)
    }

    fn visit_nft_mint<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _identifier: &SymbolicExpression,
        _recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_nft_get_owner<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, identifier)?;
        self.visit_nft_get_owner(builder, expr, token, identifier)
    }

    fn visit_nft_get_owner<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _token: &ClarityName,
        _identifier: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_let<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        bindings: &HashMap<&ClarityName, &SymbolicExpression>,
        body: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for val in bindings.values() {
            builder = self.traverse_expr(builder, val)?;
        }
        for expr in body {
            builder = self.traverse_expr(builder, expr)?;
        }
        self.visit_let(builder, expr, bindings, body)
    }

    fn visit_let<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _bindings: &HashMap<&ClarityName, &SymbolicExpression>,
        _body: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_map<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: &ClarityName,
        sequences: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for sequence in sequences {
            builder = self.traverse_expr(builder, sequence)?;
        }
        self.visit_map(builder, expr, func, sequences)
    }

    fn visit_map<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: &ClarityName,
        _sequences: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_fold<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: &ClarityName,
        sequence: &SymbolicExpression,
        initial: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, sequence)?;
        builder = self.traverse_expr(builder, initial)?;
        self.visit_fold(builder, expr, func, sequence, initial)
    }

    fn visit_fold<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: &ClarityName,
        _sequence: &SymbolicExpression,
        _initial: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_append<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        list: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, list)?;
        builder = self.traverse_expr(builder, value)?;
        self.visit_append(builder, expr, list, value)
    }

    fn visit_append<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _list: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_concat<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        lhs: &SymbolicExpression,
        rhs: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, lhs)?;
        builder = self.traverse_expr(builder, rhs)?;
        self.visit_concat(builder, expr, lhs, rhs)
    }

    fn visit_concat<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _lhs: &SymbolicExpression,
        _rhs: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_as_max_len<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        sequence: &SymbolicExpression,
        length: u128,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, sequence)?;
        self.visit_as_max_len(builder, expr, sequence, length)
    }

    fn visit_as_max_len<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _sequence: &SymbolicExpression,
        _length: u128,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_len<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        sequence: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, sequence)?;
        self.visit_len(builder, expr, sequence)
    }

    fn visit_len<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _sequence: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_element_at<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        sequence: &SymbolicExpression,
        index: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, sequence)?;
        builder = self.traverse_expr(builder, index)?;
        self.visit_element_at(builder, expr, sequence, index)
    }

    fn visit_element_at<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _sequence: &SymbolicExpression,
        _index: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_index_of<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        sequence: &SymbolicExpression,
        item: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, sequence)?;
        builder = self.traverse_expr(builder, item)?;
        self.visit_element_at(builder, expr, sequence, item)
    }

    fn visit_index_of<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _sequence: &SymbolicExpression,
        _item: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_list_cons<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for arg in args.iter() {
            builder = self.traverse_expr(builder, arg)?;
        }
        self.visit_list_cons(builder, expr, args)
    }

    fn visit_list_cons<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_call_user_defined<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for arg in args.iter() {
            builder = self.traverse_expr(builder, arg)?;
        }
        self.visit_call_user_defined(builder, expr, name, args)
    }

    fn visit_call_user_defined<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _name: &ClarityName,
        _args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_buff_cast<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_buff_cast(builder, expr, input)
    }

    fn visit_buff_cast<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_is_standard<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, value)?;
        self.visit_is_standard(builder, expr, value)
    }

    fn visit_is_standard<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_principal_destruct<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        principal: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, principal)?;
        self.visit_principal_destruct(builder, expr, principal)
    }

    fn visit_principal_destruct<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _principal: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_principal_construct<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        buff1: &SymbolicExpression,
        buff20: &SymbolicExpression,
        contract: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, buff1)?;
        builder = self.traverse_expr(builder, buff20)?;
        if let Some(contract) = contract {
            builder = self.traverse_expr(builder, contract)?;
        }
        self.visit_principal_construct(builder, expr, buff1, buff20, contract)
    }

    fn visit_principal_construct<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _buff1: &SymbolicExpression,
        _buff20: &SymbolicExpression,
        _contract: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_string_to_int<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_string_to_int(builder, expr, input)
    }

    fn visit_string_to_int<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_int_to_string<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_int_to_string(builder, expr, input)
    }

    fn visit_int_to_string<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_stx_get_account<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, owner)?;
        self.visit_stx_get_account(builder, expr, owner)
    }

    fn visit_stx_get_account<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_slice<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        seq: &SymbolicExpression,
        left: &SymbolicExpression,
        right: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, seq)?;
        builder = self.traverse_expr(builder, left)?;
        builder = self.traverse_expr(builder, right)?;
        self.visit_slice(builder, expr, seq, left, right)
    }

    fn visit_slice<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _seq: &SymbolicExpression,
        _left: &SymbolicExpression,
        _right: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_get_burn_block_info<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        prop_name: &ClarityName,
        block: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, block)?;
        self.visit_get_burn_block_info(builder, expr, prop_name, block)
    }

    fn visit_get_burn_block_info<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _prop_name: &ClarityName,
        _block: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_to_consensus_buff<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_to_consensus_buff(builder, expr, input)
    }

    fn visit_to_consensus_buff<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_from_consensus_buff<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        type_expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, type_expr)?;
        builder = self.traverse_expr(builder, input)?;
        self.visit_from_consensus_buff(builder, expr, type_expr, input)
    }

    fn visit_from_consensus_buff<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _type_expr: &SymbolicExpression,
        _input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_bitwise<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        for operand in operands {
            builder = self.traverse_expr(builder, operand)?;
        }
        self.visit_bitwise(builder, expr, func, operands)
    }

    fn visit_bitwise<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: NativeFunctions,
        _operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_bitwise_not<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        self.visit_bitwise_not(builder)
    }

    fn visit_bitwise_not<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_replace_at<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        sequence: &SymbolicExpression,
        index: &SymbolicExpression,
        element: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, sequence)?;
        builder = self.traverse_expr(builder, index)?;
        builder = self.traverse_expr(builder, element)?;
        self.visit_replace_at(builder, expr, sequence, element, index)
    }

    fn visit_replace_at<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _sequence: &SymbolicExpression,
        _index: &SymbolicExpression,
        _element: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }

    fn traverse_bit_shift<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: NativeFunctions,
        input: &SymbolicExpression,
        shamt: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder = self.traverse_expr(builder, input)?;
        builder = self.traverse_expr(builder, shamt)?;
        self.visit_bit_shift(builder, expr, func, input, shamt)
    }

    fn visit_bit_shift<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _func: NativeFunctions,
        _input: &SymbolicExpression,
        _shamt: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        Ok(builder)
    }
}

pub fn traverse<'b>(
    visitor: &mut impl ASTVisitor,
    mut builder: InstrSeqBuilder<'b>,
    exprs: &[SymbolicExpression],
) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
    for expr in exprs {
        builder = visitor.traverse_expr(builder, expr)?;
    }
    Ok(builder)
}

fn match_tuple(
    expr: &SymbolicExpression,
) -> Option<HashMap<Option<&ClarityName>, &SymbolicExpression>> {
    if let Some(list) = expr.match_list() {
        if let Some((function_name, args)) = list.split_first() {
            if let Some(function_name) = function_name.match_atom() {
                if NativeFunctions::lookup_by_name_at_version(
                    function_name,
                    &clarity::vm::ClarityVersion::latest(),
                ) == Some(NativeFunctions::TupleCons)
                {
                    let mut tuple_map = HashMap::new();
                    for element in args {
                        let pair = element.match_list().unwrap_or_default();
                        if pair.len() != 2 {
                            return None;
                        }
                        tuple_map.insert(pair[0].match_atom(), &pair[1]);
                    }
                    return Some(tuple_map);
                }
            }
        }
    }
    None
}

fn match_pairs(expr: &SymbolicExpression) -> Option<HashMap<&ClarityName, &SymbolicExpression>> {
    let list = expr.match_list()?;
    let mut tuple_map = HashMap::new();
    for pair_list in list {
        let pair = pair_list.match_list()?;
        if pair.len() != 2 {
            return None;
        }
        tuple_map.insert(pair[0].match_atom()?, &pair[1]);
    }
    Some(tuple_map)
}

fn match_pairs_list(list: &[SymbolicExpression]) -> Option<Vec<TypedVar>> {
    let mut vars = Vec::new();
    for pair_list in list {
        let pair = pair_list.match_list()?;
        if pair.len() != 2 {
            return None;
        }
        let name = pair[0].match_atom()?;
        vars.push(TypedVar {
            name,
            type_expr: &pair[1],
            decl_span: pair[0].span.clone(),
        });
    }
    Some(vars)
}
