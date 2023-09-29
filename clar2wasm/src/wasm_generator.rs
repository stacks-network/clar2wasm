use clarity::vm::{
    analysis::ContractAnalysis,
    diagnostic::DiagnosableError,
    functions::NativeFunctions,
    types::{
        CharType, FunctionType, PrincipalData, SequenceData, SequenceSubtype, StringSubtype,
        TypeSignature,
    },
    variables::NativeVariables,
    ClarityName, SymbolicExpression, SymbolicExpressionType,
};
use std::{borrow::BorrowMut, collections::HashMap};
use walrus::{
    ir::{BinaryOp, Block, InstrSeqType, LoadKind, MemArg, StoreKind, UnaryOp},
    ActiveData, DataKind, FunctionBuilder, FunctionId, GlobalId, InstrSeqBuilder, LocalId, Module,
    ValType,
};

use crate::ast_visitor::{traverse, ASTVisitor};

/// `Trap` should match the values used in the standard library and is used to
/// indicate the reason for a runtime error from the Clarity code.
#[allow(dead_code)]
#[repr(i32)]
enum Trap {
    Overflow = 0,
    Underflow = 1,
    DivideByZero = 2,
    LogOfNumberLessThanOrEqualToZero = 3,
    ExpectedANonNegativeNumber = 4,
    Panic = 5,
}

/// WasmGenerator is a Clarity AST visitor that generates a WebAssembly module
/// as it traverses the AST.
pub struct WasmGenerator<'a> {
    /// The contract analysis, which contains the expressions and type
    /// information for the contract.
    contract_analysis: &'a mut ContractAnalysis,
    /// The WebAssembly module that is being generated.
    module: Module,
    /// The error that occurred during generation, if any.
    error: Option<GeneratorError>,
    /// Offset of the end of the literal memory.
    literal_memory_end: u32,
    /// Global ID of the stack pointer.
    stack_pointer: GlobalId,
    /// Map strings saved in the literal memory to their offset.
    literal_memory_offet: HashMap<String, u32>,
    /// Map constants to an offset in the literal memory.
    constants: HashMap<String, u32>,

    /// The locals for the current function.
    locals: HashMap<String, LocalId>,
    /// Size of the current function's stack frame.
    frame_size: i32,
}

#[derive(Debug)]
pub enum GeneratorError {
    NotImplemented,
    InternalError(String),
}

impl DiagnosableError for GeneratorError {
    fn message(&self) -> String {
        match self {
            GeneratorError::NotImplemented => "Not implemented".to_string(),
            GeneratorError::InternalError(msg) => format!("Internal error: {}", msg),
        }
    }

    fn suggestion(&self) -> Option<String> {
        None
    }
}

enum FunctionKind {
    Public,
    Private,
    ReadOnly,
}

/// Return the number of bytes required to store a value of the type `ty`.
fn get_type_size(ty: &TypeSignature) -> u32 {
    match ty {
        TypeSignature::IntType | TypeSignature::UIntType => 16,
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(length))) => {
            u32::from(length)
        }
        TypeSignature::PrincipalType => {
            // Standard principal is a 1 byte version and a 20 byte Hash160.
            // Then there is an int32 for the contract name length, followed by
            // the contract name, which has a max length of 128.
            1 + 20 + 1 + 128
        }
        TypeSignature::OptionalType(inner) => 1 + get_type_size(inner),
        TypeSignature::SequenceType(SequenceSubtype::ListType(list_data)) => {
            list_data.get_max_len() * get_type_size(list_data.get_list_item_type())
        }
        TypeSignature::SequenceType(SequenceSubtype::BufferType(length)) => u32::from(length),
        _ => unimplemented!("Unsupported type: {}", ty),
    }
}

/// Return true if the value of the given type stays in memory, and false if
/// it is stored on the data stack.
fn is_in_memory_type(ty: &TypeSignature) -> bool {
    match ty {
        TypeSignature::PrincipalType | TypeSignature::SequenceType(_) => true,
        TypeSignature::IntType
        | TypeSignature::UIntType
        | TypeSignature::NoType
        | TypeSignature::BoolType
        | TypeSignature::TupleType(_)
        | TypeSignature::OptionalType(_)
        | TypeSignature::ResponseType(_) => false,
        _ => todo!("Unsupported type: {}", ty),
    }
}

/// Push a placeholder value for Wasm type `ty` onto the data stack.
fn add_placeholder_for_type(builder: &mut InstrSeqBuilder, ty: ValType) {
    match ty {
        ValType::I32 => builder.i32_const(0),
        ValType::I64 => builder.i64_const(0),
        ValType::F32 => builder.f32_const(0.0),
        ValType::F64 => builder.f64_const(0.0),
        ValType::V128 => unimplemented!("V128"),
        ValType::Externref => unimplemented!("Externref"),
        ValType::Funcref => unimplemented!("Funcref"),
    };
}

/// Push a placeholder value for Clarity type `ty` onto the data stack.
fn add_placeholder_for_clarity_type(builder: &mut InstrSeqBuilder, ty: &TypeSignature) {
    let wasm_types = clar2wasm_ty(ty);
    for wasm_type in wasm_types.iter() {
        add_placeholder_for_type(builder, *wasm_type);
    }
}

impl<'a> WasmGenerator<'a> {
    pub fn new(contract_analysis: &'a mut ContractAnalysis) -> WasmGenerator<'a> {
        let standard_lib_wasm: &[u8] = include_bytes!("standard/standard.wasm");
        let module =
            Module::from_buffer(standard_lib_wasm).expect("failed to load standard library");

        // Get the stack-pointer global ID
        let stack_pointer_name = "stack-pointer";
        let global_id = module
            .globals
            .iter()
            .find(|global| {
                global
                    .name
                    .as_ref()
                    .map_or(false, |name| name == stack_pointer_name)
            })
            .map(|global| global.id())
            .expect("Expected to find a global named $stack-pointer");

        WasmGenerator {
            contract_analysis,
            module,
            error: None,
            literal_memory_end: 0,
            stack_pointer: global_id,
            literal_memory_offet: HashMap::new(),
            constants: HashMap::new(),
            locals: HashMap::new(),
            frame_size: 0,
        }
    }

    pub fn generate(mut self) -> Result<Module, GeneratorError> {
        let expressions = std::mem::take(&mut self.contract_analysis.expressions);
        // println!("{:?}", expressions);

        let mut current_function = FunctionBuilder::new(&mut self.module.types, &[], &[]);

        if traverse(&mut self, current_function.func_body(), &expressions).is_err() {
            return Err(GeneratorError::InternalError(
                "ast traversal failed".to_string(),
            ));
        }

        self.contract_analysis.expressions = expressions;

        if let Some(err) = self.error {
            return Err(err);
        }

        // Insert a return instruction at the end of the top-level function so
        // that the top level always has no return value.
        current_function.func_body().return_();
        let top_level = current_function.finish(vec![], &mut self.module.funcs);
        self.module.exports.add(".top-level", top_level);

        // Update the initial value of the stack-pointer to point beyond the
        // literal memory.
        self.module.globals.get_mut(self.stack_pointer).kind = walrus::GlobalKind::Local(
            walrus::InitExpr::Value(walrus::ir::Value::I32(self.literal_memory_end as i32)),
        );

        Ok(self.module)
    }

    fn traverse_define_function(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
        body: &SymbolicExpression,
        kind: FunctionKind,
    ) -> Option<FunctionId> {
        let opt_function_type = match kind {
            FunctionKind::ReadOnly => {
                builder.i32_const(0);
                self.contract_analysis
                    .get_read_only_function_type(name.as_str())
            }
            FunctionKind::Public => {
                builder.i32_const(1);
                self.contract_analysis
                    .get_public_function_type(name.as_str())
            }
            FunctionKind::Private => {
                builder.i32_const(2);
                self.contract_analysis.get_private_function(name.as_str())
            }
        };
        let function_type = if let Some(FunctionType::Fixed(fixed)) = opt_function_type {
            fixed.clone()
        } else {
            self.error = Some(GeneratorError::InternalError(match opt_function_type {
                Some(_) => "expected fixed function type".to_string(),
                None => format!("unable to find function type for {}", name.as_str()),
            }));
            return None;
        };

        // Call the host interface to save this function
        // Arguments are kind (already pushed) and name (offset, length)
        let (id_offset, id_length) = self.add_identifier_string_literal(name);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Call the host interface function, `define_function`
        builder.call(
            self.module
                .funcs
                .by_name("define_function")
                .expect("define_function not found"),
        );

        let mut locals = HashMap::new();

        // Setup the parameters
        let mut param_locals = Vec::new();
        let mut params_types = Vec::new();
        for param in function_type.args.iter() {
            let param_types = clar2wasm_ty(&param.signature);
            for (n, ty) in param_types.iter().enumerate() {
                let local = self.module.locals.add(*ty);
                locals.insert(format!("{}.{}", param.name, n), local);
                param_locals.push(local);
                params_types.push(*ty);
            }
        }

        let results_types = clar2wasm_ty(&function_type.returns);
        let mut func_builder = FunctionBuilder::new(
            &mut self.module.types,
            params_types.as_slice(),
            results_types.as_slice(),
        );
        func_builder.name(name.as_str().to_string());
        let mut func_body = func_builder.func_body();

        // Function prelude
        // Save the frame pointer in a local variable.
        let frame_pointer = self.module.locals.add(ValType::I32);
        func_body
            .global_get(self.stack_pointer)
            .local_set(frame_pointer);

        // Setup the locals map for this function, saving the top-level map to
        // restore after.
        let top_level_locals = std::mem::replace(&mut self.locals, locals);

        let block = func_body.dangling_instr_seq(InstrSeqType::new(
            &mut self.module.types,
            &[],
            results_types.as_slice(),
        ));
        let block_id = block.id();

        // Traverse the body of the function
        if self.traverse_expr(block, body).is_err() {
            return None;
        }

        // TODO: We need to ensure that all exits from the function go through
        // the postlude. Maybe put the body in a block, and then have any exits
        // from the block go to the postlude with a `br` instruction?

        // Insert the function body block into the function
        func_body.instr(Block { seq: block_id });

        // Function postlude
        // Restore the initial stack pointer.
        func_body
            .local_get(frame_pointer)
            .global_set(self.stack_pointer);

        // Restore the top-level locals map.
        self.locals = top_level_locals;

        Some(func_builder.finish(param_locals, &mut self.module.funcs))
    }

    /// Gets the result type of the given `SymbolicExpression`.
    fn get_expr_type(&self, expr: &SymbolicExpression) -> Option<&TypeSignature> {
        self.contract_analysis
            .type_map
            .as_ref()
            .expect("type-checker must be called before Wasm generation")
            .get_type(expr)
    }

    /// Adds a new string literal into the memory, and returns the offset and length.
    fn add_string_literal(&mut self, s: &CharType) -> (u32, u32) {
        // If this string has already been saved in the literal memory,
        // just return the offset and length.
        if let Some(offset) = self.literal_memory_offet.get(s.to_string().as_str()) {
            return (*offset, s.to_string().len() as u32);
        }

        let data = match s {
            CharType::ASCII(s) => s.data.clone(),
            CharType::UTF8(u) => u.data.clone().into_iter().flatten().collect(),
        };
        let memory = self.module.memories.iter().next().expect("no memory found");
        let offset = self.literal_memory_end;
        let len = data.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory: memory.id(),
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            data.clone(),
        );
        self.literal_memory_end += data.len() as u32;

        // Save the offset in the literal memory for this string
        self.literal_memory_offet.insert(s.to_string(), offset);

        (offset, len)
    }

    /// Adds a new string literal into the memory for an identifier
    fn add_identifier_string_literal(&mut self, name: &clarity::vm::ClarityName) -> (u32, u32) {
        // If this identifier has already been saved in the literal memory,
        // just return the offset and length.
        if let Some(offset) = self.literal_memory_offet.get(name.as_str()) {
            return (*offset, name.len() as u32);
        }

        let memory = self.module.memories.iter().next().expect("no memory found");
        let offset = self.literal_memory_end;
        let len = name.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory: memory.id(),
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            name.as_bytes().to_vec(),
        );
        self.literal_memory_end += name.len() as u32;

        // Save the offset in the literal memory for this identifier
        self.literal_memory_offet.insert(name.to_string(), offset);

        (offset, len)
    }

    /// Adds a new literal into the memory, and returns the offset and length.
    fn add_literal(&mut self, value: &clarity::vm::Value) -> (u32, u32) {
        let data = match value {
            clarity::vm::Value::Int(i) => {
                let mut data = (((*i as u128) & 0xFFFFFFFFFFFFFFFF) as i64)
                    .to_le_bytes()
                    .to_vec();
                data.extend_from_slice(&(((*i as u128) >> 64) as i64).to_le_bytes());
                data
            }
            clarity::vm::Value::UInt(u) => {
                let mut data = ((*u & 0xFFFFFFFFFFFFFFFF) as i64).to_le_bytes().to_vec();
                data.extend_from_slice(&((*u >> 64) as i64).to_le_bytes());
                data
            }
            clarity::vm::Value::Principal(p) => match p {
                PrincipalData::Standard(standard) => {
                    let mut data = vec![standard.0];
                    data.extend_from_slice(&standard.1);
                    let contract_length = 0i32.to_le_bytes();
                    data.extend_from_slice(&contract_length);
                    data
                }
                PrincipalData::Contract(contract) => {
                    let mut data = vec![contract.issuer.0];
                    data.extend_from_slice(&contract.issuer.1);
                    let contract_length = (contract.name.len() as i32).to_le_bytes();
                    data.extend_from_slice(&contract_length);
                    data.extend_from_slice(contract.name.as_bytes());
                    data
                }
            },
            clarity::vm::Value::Sequence(SequenceData::Buffer(buff_data)) => buff_data.data.clone(),
            clarity::vm::Value::Sequence(SequenceData::String(string_data)) => {
                return self.add_string_literal(string_data);
            }
            _ => unimplemented!("Unsupported literal: {}", value),
        };
        let memory = self.module.memories.iter().next().expect("no memory found");
        let offset = self.literal_memory_end;
        let len = data.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory: memory.id(),
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            data.clone(),
        );
        self.literal_memory_end += data.len() as u32;

        (offset, len)
    }

    /// Push a new local onto the call stack, adjusting the stack pointer and
    /// tracking the current function's frame size accordingly.
    fn create_call_stack_local<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        stack_pointer: GlobalId,
        ty: &TypeSignature,
    ) -> (InstrSeqBuilder<'b>, LocalId, i32) {
        let size = get_type_size(ty) as i32;

        // Save the offset (current stack pointer) into a local
        let offset = self.module.locals.add(ValType::I32);
        builder.global_get(stack_pointer).local_tee(offset);

        // TODO: The frame stack size can be computed at compile time, so we
        //       should be able to increment the stack pointer once in the function
        //       prelude with a constant instead of incrementing it for each local.
        // (global.set $stack-pointer (i32.add (global.get $stack-pointer) (i32.const <size>))
        builder
            .i32_const(size)
            .binop(BinaryOp::I32Add)
            .global_set(stack_pointer);
        self.frame_size += size;

        (builder, offset, size)
    }

    /// Write the value that is on the top of the data stack, which has type
    /// `ty`, to the memory, at offset stored in local variable,
    /// `offset_local`, plus constant offset `offset`.
    fn write_to_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        offset_local: LocalId,
        offset: u32,
        ty: &TypeSignature,
    ) -> i32 {
        let memory = self.module.memories.iter().next().expect("no memory found");
        let size = match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Data stack: TOP | Low | High | ...
                // Save the high/low to locals.
                let high = self.module.locals.add(ValType::I64);
                let low = self.module.locals.add(ValType::I64);
                builder.local_set(low).local_set(high);

                // Store the high/low to memory.
                builder.local_get(offset_local).local_get(high).store(
                    memory.id(),
                    StoreKind::I64 { atomic: false },
                    MemArg { align: 8, offset },
                );
                builder.local_get(offset_local).local_get(low).store(
                    memory.id(),
                    StoreKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: offset + 8,
                    },
                );
                16
            }
            _ => unimplemented!("Type not yet supported for writing to memory: {ty}"),
        };
        size
    }

    /// Read a value from memory at offset stored in local variable `offset`,
    /// with type `ty`, and push it onto the top of the data stack.
    fn read_from_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        offset: LocalId,
        literal_offset: u32,
        ty: &TypeSignature,
    ) -> i32 {
        let memory = self.module.memories.iter().next().expect("no memory found");
        let size = match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Memory: Offset -> | Low | High |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: literal_offset,
                    },
                );
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: literal_offset + 8,
                    },
                );
                16
            }
            TypeSignature::OptionalType(inner) => {
                // Memory: Offset -> | Indicator | Value |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                4 + self.read_from_memory(builder, offset, literal_offset + 4, inner)
            }
            _ => unimplemented!("Type not yet supported for reading from memory: {ty}"),
        };
        size
    }

    fn traverse_statement_list<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        statements: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        assert!(
            statements.len() > 1,
            "statement list must have at least one statement"
        );
        // Traverse all but the last statement and drop any unused values.
        for stmt in &statements[..statements.len() - 1] {
            builder = self.traverse_expr(builder, stmt)?;
            // If stmt has a type, and is not the last statement, its value
            // needs to be discarded.
            if let Some(ty) = self.get_expr_type(stmt) {
                drop_value(builder.borrow_mut(), ty);
            }
        }

        // Traverse the last statement in the block, whose result is the result
        // of the `begin` expression.
        self.traverse_expr(builder, statements.last().unwrap())
    }

    /// If `name` is a reserved variable, push its value onto the data stack.
    pub fn lookup_reserved_variable<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        name: &str,
        ty: &TypeSignature,
    ) -> (InstrSeqBuilder<'b>, bool) {
        if let Some(variable) = NativeVariables::lookup_by_name_at_version(
            name,
            &self.contract_analysis.clarity_version,
        ) {
            match variable {
                NativeVariables::TxSender => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size);
                    (builder, offset, size) = self.create_call_stack_local(
                        builder,
                        self.stack_pointer,
                        &TypeSignature::PrincipalType,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `tx_sender`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("tx_sender")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::ContractCaller => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size);
                    (builder, offset, size) = self.create_call_stack_local(
                        builder,
                        self.stack_pointer,
                        &TypeSignature::PrincipalType,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `contract_caller`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("contract_caller")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::TxSponsor => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size);
                    (builder, offset, size) = self.create_call_stack_local(
                        builder,
                        self.stack_pointer,
                        &TypeSignature::PrincipalType,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `tx_sponsor`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("tx_sponsor")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::BlockHeight => {
                    // Call the host interface function, `block_height`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("block_height")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::BurnBlockHeight => {
                    // Call the host interface function, `burn_block_height`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("burn_block_height")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::NativeNone => {
                    add_placeholder_for_clarity_type(&mut builder, ty);
                    (builder, true)
                }
                NativeVariables::NativeTrue => {
                    builder.i32_const(1);
                    (builder, true)
                }
                NativeVariables::NativeFalse => {
                    builder.i32_const(0);
                    (builder, true)
                }
                NativeVariables::TotalLiquidMicroSTX => {
                    // Call the host interface function, `stx_liquid_supply`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("stx_liquid_supply")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::Regtest => {
                    // Call the host interface function, `is_in_regtest`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("is_in_regtest")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::Mainnet => {
                    // Call the host interface function, `is_in_mainnet`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("is_in_mainnet")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
                NativeVariables::ChainId => {
                    // Call the host interface function, `chain_id`
                    builder.call(
                        self.module
                            .funcs
                            .by_name("chain_id")
                            .expect("function not found"),
                    );
                    (builder, true)
                }
            }
        } else {
            (builder, false)
        }
    }

    /// If `name` is a constant, push its value onto the data stack.
    pub fn lookup_constant_variable<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        name: &str,
        ty: &TypeSignature,
    ) -> (InstrSeqBuilder<'b>, bool) {
        if let Some(offset) = self.constants.get(name) {
            // Load the offset into a local variable
            let offset_local = self.module.locals.add(ValType::I32);
            builder.i32_const(*offset as i32).local_set(offset_local);

            // If `ty` is a value that stays in memory, we can just push the
            // offset and length to the stack.
            if is_in_memory_type(ty) {
                builder
                    .local_get(offset_local)
                    .i32_const(get_type_size(ty) as i32);
                (builder, true)
            } else {
                // Otherwise, we need to load the value from memory.
                self.read_from_memory(&mut builder, offset_local, 0, ty);
                (builder, true)
            }
        } else {
            (builder, false)
        }
    }
}

impl ASTVisitor for WasmGenerator<'_> {
    fn traverse_arithmetic<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        func: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let ty = self
            .get_expr_type(expr)
            .expect("arithmetic expression must be typed");
        let type_suffix = match ty {
            TypeSignature::IntType => "int",
            TypeSignature::UIntType => "uint",
            _ => {
                self.error = Some(GeneratorError::InternalError(
                    "invalid type for arithmetic".to_string(),
                ));
                return Err(builder);
            }
        };
        let helper_func = match func {
            NativeFunctions::Add => self
                .module
                .funcs
                .by_name(&format!("add-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: add-{type_suffix}")),
            NativeFunctions::Subtract => self
                .module
                .funcs
                .by_name(&format!("sub-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: sub-{type_suffix}")),
            NativeFunctions::Multiply => self
                .module
                .funcs
                .by_name(&format!("mul-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: mul-{type_suffix}")),
            NativeFunctions::Divide => self
                .module
                .funcs
                .by_name(&format!("div-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: div-{type_suffix}")),
            NativeFunctions::Modulo => self
                .module
                .funcs
                .by_name(&format!("mod-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: mod-{type_suffix}")),
            NativeFunctions::Log2 => self
                .module
                .funcs
                .by_name(&format!("log2-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: log2-{type_suffix}")),
            NativeFunctions::Sqrti => self
                .module
                .funcs
                .by_name(&format!("sqrti-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: sqrti-{type_suffix}")),
            NativeFunctions::Power => self
                .module
                .funcs
                .by_name(&format!("pow-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: pow-{type_suffix}")),
            _ => {
                self.error = Some(GeneratorError::NotImplemented);
                return Err(builder);
            }
        };

        // Start off with operand 0, then loop over the rest, calling the
        // helper function with a pair of operands, either operand 0 and 1, or
        // the result of the previous call and the next operand.
        // e.g. (+ 1 2 3 4) becomes (+ (+ (+ 1 2) 3) 4)
        builder = self.traverse_expr(builder, &operands[0])?;
        for operand in operands.iter().skip(1) {
            builder = self.traverse_expr(builder, operand)?;
            builder.call(helper_func);
        }

        Ok(builder)
    }

    fn traverse_bitwise<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        func: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let helper_func = match func {
            NativeFunctions::BitwiseAnd => self
                .module
                .funcs
                .by_name("bit-and")
                .unwrap_or_else(|| panic!("function not found: bit-and")),
            NativeFunctions::BitwiseOr => self
                .module
                .funcs
                .by_name("bit-or")
                .unwrap_or_else(|| panic!("function not found: bit-or")),
            NativeFunctions::BitwiseXor2 => self
                .module
                .funcs
                .by_name("bit-xor")
                .unwrap_or_else(|| panic!("function not found: bit-xor")),
            _ => {
                self.error = Some(GeneratorError::NotImplemented);
                return Err(builder);
            }
        };

        // Start off with operand 0, then loop over the rest, calling the
        // helper function with a pair of operands, either operand 0 and 1, or
        // the result of the previous call and the next operand.
        // e.g. (+ 1 2 3 4) becomes (+ (+ (+ 1 2) 3) 4)
        builder = self.traverse_expr(builder, &operands[0])?;
        for operand in operands.iter().skip(1) {
            builder = self.traverse_expr(builder, operand)?;
            builder.call(helper_func);
        }

        Ok(builder)
    }

    fn visit_bit_shift<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        func: NativeFunctions,
        input: &SymbolicExpression,
        _shamt: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let ty = self
            .get_expr_type(input)
            .expect("bit shift operands must be typed");
        let type_suffix = match ty {
            TypeSignature::IntType => "int",
            TypeSignature::UIntType => "uint",
            _ => {
                self.error = Some(GeneratorError::InternalError(
                    "invalid type for shift".to_string(),
                ));
                return Err(builder);
            }
        };
        let helper_func = match func {
            NativeFunctions::BitwiseLShift => self
                .module
                .funcs
                .by_name("bit-shift-left")
                .unwrap_or_else(|| panic!("function not found: bit-shift-left")),
            NativeFunctions::BitwiseRShift => self
                .module
                .funcs
                .by_name(&format!("bit-shift-right-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: bit-shift-right-{type_suffix}")),
            _ => {
                self.error = Some(GeneratorError::NotImplemented);
                return Err(builder);
            }
        };
        builder.call(helper_func);

        Ok(builder)
    }

    fn visit_bitwise_not<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let helper_func = self
            .module
            .funcs
            .by_name("bit-not")
            .unwrap_or_else(|| panic!("function not found: bit-not"));
        builder.call(helper_func);
        Ok(builder)
    }

    fn visit_comparison<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        func: NativeFunctions,
        operands: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let ty = self
            .get_expr_type(&operands[0])
            .expect("comparison operands must be typed");
        let type_suffix = match ty {
            TypeSignature::IntType => "int",
            TypeSignature::UIntType => "uint",
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(_))) => {
                "string-ascii"
            }
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => {
                "string-utf8"
            }
            TypeSignature::SequenceType(SequenceSubtype::BufferType(_)) => "buffer",
            _ => {
                self.error = Some(GeneratorError::InternalError(
                    "invalid type for comparison".to_string(),
                ));
                return Err(builder);
            }
        };
        let helper_func = match func {
            NativeFunctions::CmpLess => self
                .module
                .funcs
                .by_name(&format!("lt-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: lt-{type_suffix}")),
            NativeFunctions::CmpGreater => self
                .module
                .funcs
                .by_name(&format!("gt-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: gt-{type_suffix}")),
            NativeFunctions::CmpLeq => self
                .module
                .funcs
                .by_name(&format!("le-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: le-{type_suffix}")),
            NativeFunctions::CmpGeq => self
                .module
                .funcs
                .by_name(&format!("ge-{type_suffix}"))
                .unwrap_or_else(|| panic!("function not found: ge-{type_suffix}")),
            _ => {
                self.error = Some(GeneratorError::NotImplemented);
                return Err(builder);
            }
        };
        builder.call(helper_func);

        Ok(builder)
    }

    fn visit_literal_value<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        value: &clarity::vm::Value,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        match value {
            clarity::vm::Value::Int(i) => {
                builder.i64_const((i & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const(((i >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(builder)
            }
            clarity::vm::Value::UInt(u) => {
                builder.i64_const((u & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const(((u >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(builder)
            }
            clarity::vm::Value::Sequence(SequenceData::String(s)) => {
                let (offset, len) = self.add_string_literal(s);
                builder.i32_const(offset as i32);
                builder.i32_const(len as i32);
                Ok(builder)
            }
            clarity::vm::Value::Principal(_)
            | clarity::vm::Value::Sequence(SequenceData::Buffer(_)) => {
                let (offset, len) = self.add_literal(value);
                builder.i32_const(offset as i32);
                builder.i32_const(len as i32);
                Ok(builder)
            }
            _ => {
                self.error = Some(GeneratorError::NotImplemented);
                Err(builder)
            }
        }
    }

    fn visit_atom<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        atom: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let ty = match self.get_expr_type(expr) {
            Some(ty) => ty.clone(),
            None => {
                self.error = Some(GeneratorError::InternalError(
                    "atom expression must be typed".to_string(),
                ));
                return Err(builder);
            }
        };

        // Handle builtin variables
        let is_builtin: bool;
        (builder, is_builtin) = self.lookup_reserved_variable(builder, atom.as_str(), &ty);
        if is_builtin {
            return Ok(builder);
        }

        // Handle constants
        let is_constant: bool;
        (builder, is_constant) = self.lookup_constant_variable(builder, atom.as_str(), &ty);
        if is_constant {
            return Ok(builder);
        }

        let types = clar2wasm_ty(&ty);
        for n in 0..types.len() {
            let local = match self.locals.get(format!("{}.{}", atom.as_str(), n).as_str()) {
                Some(local) => *local,
                None => {
                    self.error = Some(GeneratorError::InternalError(format!(
                        "unable to find local for {}",
                        atom.as_str()
                    )));
                    return Err(builder);
                }
            };
            builder.local_get(local);
        }

        Ok(builder)
    }

    fn traverse_define_private<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        _parameters: Option<Vec<crate::ast_visitor::TypedVar<'_>>>,
        body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if self
            .traverse_define_function(&mut builder, name, body, FunctionKind::Private)
            .is_some()
        {
            Ok(builder)
        } else {
            Err(builder)
        }
    }

    fn traverse_define_read_only<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        _parameters: Option<Vec<crate::ast_visitor::TypedVar<'_>>>,
        body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if let Some(function_id) =
            self.traverse_define_function(&mut builder, name, body, FunctionKind::ReadOnly)
        {
            self.module.exports.add(name.as_str(), function_id);
            Ok(builder)
        } else {
            Err(builder)
        }
    }

    fn traverse_define_public<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        _parameters: Option<Vec<crate::ast_visitor::TypedVar<'_>>>,
        body: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if let Some(function_id) =
            self.traverse_define_function(&mut builder, name, body, FunctionKind::Public)
        {
            self.module.exports.add(name.as_str(), function_id);
            Ok(builder)
        } else {
            Err(builder)
        }
    }

    fn traverse_define_data_var<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        _data_type: &SymbolicExpression,
        initial: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Store the identifier as a string literal in the memory
        let (name_offset, name_length) = self.add_identifier_string_literal(name);

        // The initial value can be placed on the top of the memory, since at
        // the top-level, we have not set up the call stack yet.
        let ty = self
            .get_expr_type(initial)
            .expect("initial value expression must be typed")
            .clone();
        let offset = self.module.locals.add(ValType::I32);
        builder
            .i32_const(self.literal_memory_end as i32)
            .local_set(offset);

        // Traverse the initial value for the data variable (result is on the
        // data stack)
        builder = self.traverse_expr(builder, initial)?;

        // Write the initial value to the memory, to be read by the host.
        let size = self.write_to_memory(builder.borrow_mut(), offset, 0, &ty);

        // Increment the literal memory end
        // FIXME: These initial values do not need to be saved in the literal
        //        memory forever... we just need them once, when .top-level
        //        is called.
        self.literal_memory_end += size as u32;

        // Push the name onto the data stack
        builder
            .i32_const(name_offset as i32)
            .i32_const(name_length as i32);

        // Push the offset onto the data stack
        builder.local_get(offset);

        // Push the size onto the data stack
        builder.i32_const(size);

        // Call the host interface function, `define_variable`
        builder.call(
            self.module
                .funcs
                .by_name("define_variable")
                .expect("function not found"),
        );
        Ok(builder)
    }

    fn traverse_define_ft<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        supply: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Store the identifier as a string literal in the memory
        let (name_offset, name_length) = self.add_identifier_string_literal(name);

        // Push the name onto the data stack
        builder
            .i32_const(name_offset as i32)
            .i32_const(name_length as i32);

        // Push the supply to the stack, as an optional uint
        // (first i32 indicates some/none)
        if let Some(supply) = supply {
            builder.i32_const(1);
            builder = self.traverse_expr(builder, supply)?;
        } else {
            builder.i32_const(0).i64_const(0).i64_const(0);
        }

        builder.call(
            self.module
                .funcs
                .by_name("define_ft")
                .expect("function not found"),
        );
        Ok(builder)
    }

    fn traverse_define_nft<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        _nft_type: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Store the identifier as a string literal in the memory
        let (name_offset, name_length) = self.add_identifier_string_literal(name);

        // Push the name onto the data stack
        builder
            .i32_const(name_offset as i32)
            .i32_const(name_length as i32);

        builder.call(
            self.module
                .funcs
                .by_name("define_nft")
                .expect("function not found"),
        );
        Ok(builder)
    }

    fn traverse_define_constant<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // If the initial value is a literal, then we can directly add it to
        // the literal memory.
        let offset = if let SymbolicExpressionType::LiteralValue(value) = &value.expr {
            let (offset, _len) = self.add_literal(value);
            offset
        } else {
            // If the initial expression is not a literal, then we need to
            // reserve the space for it, and then execute the expression and
            // write the result into the reserved space.
            let offset = self.literal_memory_end;
            let offset_local = self.module.locals.add(ValType::I32);
            builder.i32_const(offset as i32).local_set(offset_local);

            let ty = self
                .get_expr_type(value)
                .expect("constant value must be typed")
                .clone();

            let len = get_type_size(&ty);
            self.literal_memory_end += len;

            // Traverse the initial value expression.
            builder = self.traverse_expr(builder, value)?;

            // Write the result (on the stack) to the memory
            self.write_to_memory(&mut builder, offset_local, 0, &ty);

            offset
        };

        self.constants.insert(name.to_string(), offset);

        Ok(builder)
    }

    fn visit_define_map<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        _key_type: &SymbolicExpression,
        _value_type: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Store the identifier as a string literal in the memory
        let (name_offset, name_length) = self.add_identifier_string_literal(name);

        // Push the name onto the data stack
        builder
            .i32_const(name_offset as i32)
            .i32_const(name_length as i32);

        builder.call(
            self.module
                .funcs
                .by_name("define_map")
                .expect("function not found"),
        );
        Ok(builder)
    }

    fn traverse_begin<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        statements: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        self.traverse_statement_list(builder, statements)
    }

    fn traverse_some<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // (some <val>) is represented by an i32 1, followed by the value
        builder.i32_const(1);
        builder = self.traverse_expr(builder, value)?;
        Ok(builder)
    }

    fn traverse_ok<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // (ok <val>) is represented by an i32 1, followed by the ok value,
        // followed by a placeholder for the err value
        builder.i32_const(1);
        builder = self.traverse_expr(builder, value)?;
        let ty = self
            .get_expr_type(expr)
            .expect("ok expression must be typed");
        if let TypeSignature::ResponseType(inner_types) = ty {
            let err_types = clar2wasm_ty(&inner_types.1);
            for err_type in err_types.iter() {
                add_placeholder_for_type(&mut builder, *err_type);
            }
        } else {
            panic!("expected response type");
        }
        Ok(builder)
    }

    fn traverse_err<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // (err <val>) is represented by an i32 0, followed by a placeholder
        // for the ok value, followed by the err value
        builder.i32_const(0);
        let ty = self
            .get_expr_type(expr)
            .expect("err expression must be typed");
        if let TypeSignature::ResponseType(inner_types) = ty {
            let ok_types = clar2wasm_ty(&inner_types.0);
            for ok_type in ok_types.iter() {
                add_placeholder_for_type(&mut builder, *ok_type);
            }
        } else {
            panic!("expected response type");
        }
        self.traverse_expr(builder, value)
    }

    fn visit_call_user_defined<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        _args: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        builder.call(
            self.module
                .funcs
                .by_name(name.as_str())
                .expect("function not found"),
        );
        Ok(builder)
    }

    fn traverse_concat<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        lhs: &SymbolicExpression,
        rhs: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Create a new sequence to hold the result in the stack frame
        let ty = self
            .get_expr_type(expr)
            .expect("concat expression must be typed")
            .clone();
        let offset;
        (builder, offset, _) = self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Traverse the lhs, leaving it on the data stack (offset, size)
        builder = self.traverse_expr(builder, lhs)?;

        // Retrieve the memcpy function:
        // memcpy(src_offset, length, dst_offset)
        let memcpy = self
            .module
            .funcs
            .by_name("memcpy")
            .expect("function not found: memcpy");

        // Copy the lhs to the new sequence
        builder.local_get(offset).call(memcpy);

        // Save the new destination offset
        let end_offset = self.module.locals.add(ValType::I32);
        builder.local_set(end_offset);

        // Traverse the rhs, leaving it on the data stack (offset, size)
        builder = self.traverse_expr(builder, rhs)?;

        // Copy the rhs to the new sequence
        builder.local_get(end_offset).call(memcpy);

        // Total size = end_offset - offset
        let size = self.module.locals.add(ValType::I32);
        builder
            .local_get(offset)
            .binop(BinaryOp::I32Sub)
            .local_set(size);

        // Return the new sequence (offset, size)
        builder.local_get(offset).local_get(size);

        Ok(builder)
    }

    fn visit_var_get<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Get the offset and length for this identifier in the literal memory
        let id_offset = *self
            .literal_memory_offet
            .get(name.as_str())
            .expect("variable not found: {name}");
        let id_length = name.len();

        // Create a new local to hold the result on the call stack
        let ty = self
            .get_expr_type(expr)
            .expect("var-get expression must be typed")
            .clone();
        let (offset, size);
        (builder, offset, size) = self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the offset and size to the data stack
        builder.local_get(offset).i32_const(size);

        // Call the host interface function, `get_variable`
        builder.call(
            self.module
                .funcs
                .by_name("get_variable")
                .expect("function not found"),
        );

        // Host interface fills the result into the specified memory. Read it
        // back out, and place the value on the data stack.
        self.read_from_memory(builder.borrow_mut(), offset, 0, &ty);

        Ok(builder)
    }

    fn visit_var_set<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Get the offset and length for this identifier in the literal memory
        let id_offset = *self
            .literal_memory_offet
            .get(name.as_str())
            .expect("variable not found: {name}");
        let id_length = name.len();

        // Create space on the call stack to write the value
        let ty = self
            .get_expr_type(value)
            .expect("var-set value expression must be typed")
            .clone();
        let (offset, size);
        (builder, offset, size) = self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Write the value to the memory (it's already on the data stack)
        self.write_to_memory(builder.borrow_mut(), offset, 0, &ty);

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the offset and size to the data stack
        builder.local_get(offset).i32_const(size);

        // Call the host interface function, `set_variable`
        builder.call(
            self.module
                .funcs
                .by_name("set_variable")
                .expect("function not found"),
        );

        // `var-set` always returns `true`
        builder.i32_const(1);

        Ok(builder)
    }

    fn traverse_list_cons<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        list: &[SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let ty = self
            .get_expr_type(expr)
            .expect("list expression must be typed")
            .clone();
        let (elem_ty, num_elem) =
            if let TypeSignature::SequenceType(SequenceSubtype::ListType(list_type)) = &ty {
                (list_type.get_list_item_type(), list_type.get_max_len())
            } else {
                panic!(
                    "Expected list type for list expression, but found: {:?}",
                    ty
                );
            };

        assert_eq!(num_elem as usize, list.len(), "list size mismatch");

        // Allocate space on the data stack for the entire list
        let (offset, size);
        (builder, offset, size) = self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Loop through the expressions in the list and store them onto the
        // data stack.
        let mut total_size = 0;
        for expr in list.iter() {
            builder = self.traverse_expr(builder, expr)?;
            let elem_size = self.write_to_memory(builder.borrow_mut(), offset, total_size, elem_ty);
            total_size += elem_size as u32;
        }
        assert_eq!(total_size, size as u32, "list size mismatch");

        // Push the offset and size to the data stack
        builder.local_get(offset).i32_const(size);

        Ok(builder)
    }

    fn traverse_fold<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        func: &ClarityName,
        sequence: &SymbolicExpression,
        initial: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Fold takes an initial value, and a sequence, and applies a function
        // to the output of the previous call, or the initial value in the case
        // of the first call, and each element of the sequence.
        // ```
        // (fold - (list 2 4 6) 0)
        // ```
        // is equivalent to
        // ```
        // (- 6 (- 4 (- 2 0)))
        // ```

        // The result type must match the type of the initial value
        let result_clar_ty = self
            .get_expr_type(initial)
            .expect("fold's initial value expression must be typed");
        let result_ty = clar2wasm_ty(result_clar_ty);
        let loop_body_ty = InstrSeqType::new(&mut self.module.types, &[], &[]);

        // Get the type of the sequence
        let seq_ty = match self
            .get_expr_type(sequence)
            .expect("sequence expression must be typed")
        {
            TypeSignature::SequenceType(seq_ty) => seq_ty.clone(),
            _ => {
                self.error = Some(GeneratorError::InternalError(
                    "expected sequence type".to_string(),
                ));
                return Err(builder);
            }
        };

        let (seq_len, elem_ty) = match &seq_ty {
            SequenceSubtype::ListType(list_type) => {
                (list_type.get_max_len(), list_type.get_list_item_type())
            }
            _ => unimplemented!("Unsupported sequence type"),
        };

        // Evaluate the sequence, which will load it into the call stack,
        // leaving the offset and size on the data stack.
        builder = self.traverse_expr(builder, sequence)?;

        // Drop the size, since we don't need it
        builder.drop();

        // Store the offset into a local
        let offset = self.module.locals.add(ValType::I32);
        builder.local_set(offset);

        let elem_size = get_type_size(elem_ty);

        // Store the end of the sequence into a local
        let end_offset = self.module.locals.add(ValType::I32);
        builder
            .local_get(offset)
            .i32_const((seq_len * elem_size) as i32)
            .binop(BinaryOp::I32Add)
            .local_set(end_offset);

        // Evaluate the initial value, so that its result is on the data stack
        builder = self.traverse_expr(builder, initial)?;

        if seq_len == 0 {
            // If the sequence is empty, just return the initial value
            return Ok(builder);
        }

        // Define local(s) to hold the intermediate result, and initialize them
        // with the initial value. Not that we are looping in reverse order, to
        // pop values from the top of the stack.
        let mut result_locals = Vec::with_capacity(result_ty.len());
        for local_ty in result_ty.iter().rev() {
            let local = self.module.locals.add(*local_ty);
            result_locals.push(local);
            builder.local_set(local);
        }
        result_locals.reverse();

        // Define the body of a loop, to loop over the sequence and make the
        // function call.
        builder.loop_(loop_body_ty, |loop_| {
            let loop_id = loop_.id();

            // Load the element from the sequence
            let elem_size = self.read_from_memory(loop_, offset, 0, elem_ty);

            // Push the locals to the stack
            for result_local in result_locals.iter() {
                loop_.local_get(*result_local);
            }

            // Call the function
            loop_.call(
                self.module
                    .funcs
                    .by_name(func.as_str())
                    .expect("function not found"),
            );

            // Save the result into the locals (in reverse order as we pop)
            for result_local in result_locals.iter().rev() {
                loop_.local_set(*result_local);
            }

            // Increment the offset by the size of the element, leaving the
            // offset on the top of the stack
            loop_
                .local_get(offset)
                .i32_const(elem_size)
                .binop(BinaryOp::I32Add)
                .local_tee(offset);

            // Loop if we haven't reached the end of the sequence
            loop_
                .local_get(end_offset)
                .binop(BinaryOp::I32LtU)
                .br_if(loop_id);
        });

        // Push the locals to the stack
        for result_local in result_locals.iter() {
            builder.local_get(*result_local);
        }

        Ok(builder)
    }

    fn traverse_as_contract<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        inner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Call the host interface function, `enter_as_contract`
        builder.call(
            self.module
                .funcs
                .by_name("enter_as_contract")
                .expect("enter_as_contract not found"),
        );

        // Traverse the inner expression
        builder = self.traverse_expr(builder, inner)?;

        // Call the host interface function, `exit_as_contract`
        builder.call(
            self.module
                .funcs
                .by_name("exit_as_contract")
                .expect("exit_as_contract not found"),
        );

        Ok(builder)
    }

    fn visit_stx_get_balance<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Owner is on the stack, so just call the host interface function,
        // `stx_get_balance`
        builder.call(
            self.module
                .funcs
                .by_name("stx_get_balance")
                .expect("stx_get_balance not found"),
        );
        Ok(builder)
    }

    fn visit_stx_get_account<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Owner is on the stack, so just call the host interface function,
        // `stx_get_account`
        builder.call(
            self.module
                .funcs
                .by_name("stx_account")
                .expect("stx_account not found"),
        );
        Ok(builder)
    }

    fn visit_stx_burn<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _amount: &SymbolicExpression,
        _sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Amount and sender are on the stack, so just call the host interface
        // function, `stx_burn`
        builder.call(
            self.module
                .funcs
                .by_name("stx_burn")
                .expect("stx_burn not found"),
        );
        Ok(builder)
    }

    fn visit_stx_transfer<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        _amount: &SymbolicExpression,
        _sender: &SymbolicExpression,
        _recipient: &SymbolicExpression,
        _memo: Option<&SymbolicExpression>,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Amount, sender, and recipient are on the stack. If memo is none, we
        // need to add a placeholder to the stack, then we can call the host
        // interface function, `stx_transfer`
        if _memo.is_none() {
            builder.i32_const(0).i32_const(0);
        }
        builder.call(
            self.module
                .funcs
                .by_name("stx_transfer")
                .expect("stx_transfer not found"),
        );
        Ok(builder)
    }

    fn visit_ft_get_supply<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack, then call the host interface
        // function `ft_get_supply`
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        builder.call(
            self.module
                .funcs
                .by_name("ft_get_supply")
                .expect("ft_get_supply not found"),
        );

        Ok(builder)
    }

    fn traverse_ft_get_balance<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        owner: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the owner onto the stack
        builder = self.traverse_expr(builder, owner)?;

        // Call the host interface function `ft_get_balance`
        builder.call(
            self.module
                .funcs
                .by_name("ft_get_balance")
                .expect("ft_get_balance not found"),
        );

        Ok(builder)
    }

    fn traverse_ft_burn<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        amount: &SymbolicExpression,
        sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the amount and sender onto the stack
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, sender)?;

        // Call the host interface function `ft_burn`
        builder.call(
            self.module
                .funcs
                .by_name("ft_burn")
                .expect("ft_burn not found"),
        );

        Ok(builder)
    }

    fn traverse_ft_mint<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        amount: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the amount and recipient onto the stack
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, recipient)?;

        // Call the host interface function `ft_mint`
        builder.call(
            self.module
                .funcs
                .by_name("ft_mint")
                .expect("ft_mint not found"),
        );

        Ok(builder)
    }

    fn traverse_ft_transfer<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        amount: &SymbolicExpression,
        sender: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the amount, sender, and recipient onto the stack
        builder = self.traverse_expr(builder, amount)?;
        builder = self.traverse_expr(builder, sender)?;
        builder = self.traverse_expr(builder, recipient)?;

        // Call the host interface function `ft_transfer`
        builder.call(
            self.module
                .funcs
                .by_name("ft_transfer")
                .expect("ft_transfer not found"),
        );

        Ok(builder)
    }

    fn traverse_nft_get_owner<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the identifier onto the stack
        builder = self.traverse_expr(builder, identifier)?;

        let identifier_ty = self
            .get_expr_type(identifier)
            .expect("NFT identifier must be typed")
            .clone();

        // Allocate space on the stack for the identifier
        let id_offset;
        let id_size;
        (builder, id_offset, id_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &identifier_ty);

        // Write the identifier to the stack (since the host needs to handle generic types)
        self.write_to_memory(&mut builder, id_offset, 0, &identifier_ty);

        // Push the offset and size to the data stack
        builder.local_get(id_offset).i32_const(id_size);

        // Reserve stack space for the return value, a principal
        let return_offset;
        let return_size;
        (builder, return_offset, return_size) = self.create_call_stack_local(
            builder,
            self.stack_pointer,
            &TypeSignature::PrincipalType,
        );

        // Push the offset and size to the data stack
        builder.local_get(return_offset).i32_const(return_size);

        // Call the host interface function `nft_get_owner`
        builder.call(
            self.module
                .funcs
                .by_name("nft_get_owner")
                .expect("nft_get_owner not found"),
        );

        Ok(builder)
    }

    fn traverse_nft_burn<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
        sender: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the identifier onto the stack
        builder = self.traverse_expr(builder, identifier)?;

        let identifier_ty = self
            .get_expr_type(identifier)
            .expect("NFT identifier must be typed")
            .clone();

        // Allocate space on the stack for the identifier
        let id_offset;
        let id_size;
        (builder, id_offset, id_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &identifier_ty);

        // Write the identifier to the stack (since the host needs to handle generic types)
        self.write_to_memory(&mut builder, id_offset, 0, &identifier_ty);

        // Push the offset and size to the data stack
        builder.local_get(id_offset).i32_const(id_size);

        // Push the sender onto the stack
        builder = self.traverse_expr(builder, sender)?;

        // Call the host interface function `nft_burn`
        builder.call(
            self.module
                .funcs
                .by_name("nft_burn")
                .expect("nft_burn not found"),
        );

        Ok(builder)
    }

    fn traverse_nft_mint<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the identifier onto the stack
        builder = self.traverse_expr(builder, identifier)?;

        let identifier_ty = self
            .get_expr_type(identifier)
            .expect("NFT identifier must be typed")
            .clone();

        // Allocate space on the stack for the identifier
        let id_offset;
        let id_size;
        (builder, id_offset, id_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &identifier_ty);

        // Write the identifier to the stack (since the host needs to handle generic types)
        self.write_to_memory(&mut builder, id_offset, 0, &identifier_ty);

        // Push the offset and size to the data stack
        builder.local_get(id_offset).i32_const(id_size);

        // Push the recipient onto the stack
        builder = self.traverse_expr(builder, recipient)?;

        // Call the host interface function `nft_mint`
        builder.call(
            self.module
                .funcs
                .by_name("nft_mint")
                .expect("nft_mint not found"),
        );

        Ok(builder)
    }

    fn traverse_nft_transfer<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        token: &ClarityName,
        identifier: &SymbolicExpression,
        sender: &SymbolicExpression,
        recipient: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Push the token name onto the stack
        let (id_offset, id_length) = self.add_identifier_string_literal(token);
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Push the identifier onto the stack
        builder = self.traverse_expr(builder, identifier)?;

        let identifier_ty = self
            .get_expr_type(identifier)
            .expect("NFT identifier must be typed")
            .clone();

        // Allocate space on the stack for the identifier
        let id_offset;
        let id_size;
        (builder, id_offset, id_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &identifier_ty);

        // Write the identifier to the stack (since the host needs to handle generic types)
        self.write_to_memory(&mut builder, id_offset, 0, &identifier_ty);

        // Push the offset and size to the data stack
        builder.local_get(id_offset).i32_const(id_size);

        // Push the sender onto the stack
        builder = self.traverse_expr(builder, sender)?;

        // Push the recipient onto the stack
        builder = self.traverse_expr(builder, recipient)?;

        // Call the host interface function `nft_transfer`
        builder.call(
            self.module
                .funcs
                .by_name("nft_transfer")
                .expect("nft_transfer not found"),
        );

        Ok(builder)
    }

    fn visit_unwrap_panic<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        input: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // There must be either an `optional` or a `response` on the top of the
        // stack. Both use an i32 indicator, where 0 means `none` or `err`. In
        // both cases, if this indicator is a 0, then we need to early exit.

        // Get the type of the input expression
        let input_ty = self
            .get_expr_type(input)
            .expect("try input expression must be typed")
            .clone();

        match &input_ty {
            TypeSignature::OptionalType(val_ty) => {
                // For the optional case, e.g. `(unwrap-panic (some 1))`, the stack
                // will look like:
                // 1 -- some value
                // 1 -- indicator
                // We need to get to the indicator, so we can pop the some value amd
                // store it in a local, then check the indicator. If it's 0, we need to
                // trigger a runtime error. If it's a 1, we just push the some value
                // back onto the stack and continue execution.

                // Save the value in locals
                let wasm_types = clar2wasm_ty(val_ty);
                let mut val_locals = Vec::with_capacity(wasm_types.len());
                for local_ty in wasm_types.iter().rev() {
                    let local = self.module.locals.add(*local_ty);
                    val_locals.push(local);
                    builder.local_set(local);
                }

                // If the indicator is 0, throw a runtime error
                builder.unop(UnaryOp::I32Eqz).if_else(
                    InstrSeqType::new(&mut self.module.types, &[], &[]),
                    |then| {
                        then.i32_const(Trap::Panic as i32).call(
                            self.module
                                .funcs
                                .by_name("runtime-error")
                                .expect("runtime_error not found"),
                        );
                    },
                    |_| {},
                );

                // Otherwise, push the value back onto the stack
                for &val_local in val_locals.iter().rev() {
                    builder.local_get(val_local);
                }

                Ok(builder)
            }
            TypeSignature::ResponseType(inner_types) => {
                // Ex. `(unwrap-panic (ok 1))`, where the value type is
                // `(response uint uint)`, the stack will look like:
                // 0 -- err value
                // 1 -- ok value
                // 1 -- indicator
                // We need to get to the indicator, so we can drop the err value, since
                // that is not needed, then we can pop the ok value and store them in a
                // local. Now we can check the indicator. If it's 0, we need to trigger
                // a runtime error. If it's a 1, we just push the ok value back onto
                // the stack and continue execution.

                let (ok_ty, err_ty) = &**inner_types;

                // Drop the err value
                drop_value(&mut builder, err_ty);

                // Save the ok value in locals
                let ok_wasm_types = clar2wasm_ty(ok_ty);
                let mut ok_val_locals = Vec::with_capacity(ok_wasm_types.len());
                for local_ty in ok_wasm_types.iter().rev() {
                    let local = self.module.locals.add(*local_ty);
                    ok_val_locals.push(local);
                    builder.local_set(local);
                }

                // If the indicator is 0, throw a runtime error
                builder.unop(UnaryOp::I32Eqz).if_else(
                    InstrSeqType::new(&mut self.module.types, &[], &[]),
                    |then| {
                        then.i32_const(Trap::Panic as i32).call(
                            self.module
                                .funcs
                                .by_name("runtime-error")
                                .expect("runtime_error not found"),
                        );
                    },
                    |_| {},
                );

                // Otherwise, push the value back onto the stack
                for &val_local in ok_val_locals.iter().rev() {
                    builder.local_get(val_local);
                }

                Ok(builder)
            }
            _ => Err(builder),
        }
    }

    fn traverse_map_get<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Get the offset and length for this identifier in the literal memory
        let id_offset = *self
            .literal_memory_offet
            .get(name.as_str())
            .expect("map not found: {name}");
        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Create space on the call stack to write the key
        let ty = self
            .get_expr_type(key)
            .expect("map-set value expression must be typed")
            .clone();
        let (key_offset, key_size);
        (builder, key_offset, key_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the key to the data stack
        builder = self.traverse_expr(builder, key)?;

        // Write the key to the memory (it's already on the data stack)
        self.write_to_memory(builder.borrow_mut(), key_offset, 0, &ty);

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size);

        // Create a new local to hold the result on the call stack
        let ty = self
            .get_expr_type(expr)
            .expect("map-get? expression must be typed")
            .clone();
        let (return_offset, return_size);
        (builder, return_offset, return_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the return value offset and size to the data stack
        builder.local_get(return_offset).i32_const(return_size);

        // Call the host-interface function, `map_get`
        builder.call(
            self.module
                .funcs
                .by_name("map_get")
                .expect("map_get not found"),
        );

        // Host interface fills the result into the specified memory. Read it
        // back out, and place the value on the data stack.
        self.read_from_memory(builder.borrow_mut(), return_offset, 0, &ty);

        Ok(builder)
    }

    fn traverse_map_set<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Get the offset and length for this identifier in the literal memory
        let id_offset = *self
            .literal_memory_offet
            .get(name.as_str())
            .expect("map not found: {name}");
        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Create space on the call stack to write the key
        let ty = self
            .get_expr_type(key)
            .expect("map-set value expression must be typed")
            .clone();
        let (key_offset, key_size);
        (builder, key_offset, key_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the key to the data stack
        builder = self.traverse_expr(builder, key)?;

        // Write the key to the memory (it's already on the data stack)
        self.write_to_memory(builder.borrow_mut(), key_offset, 0, &ty);

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size);

        // Create space on the call stack to write the value
        let ty = self
            .get_expr_type(value)
            .expect("map-set value expression must be typed")
            .clone();
        let (val_offset, val_size);
        (builder, val_offset, val_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the value to the data stack
        builder = self.traverse_expr(builder, value)?;

        // Write the value to the memory (it's already on the data stack)
        self.write_to_memory(builder.borrow_mut(), val_offset, 0, &ty);

        // Push the value offset and size to the data stack
        builder.local_get(val_offset).i32_const(val_size);

        // Call the host interface function, `set_variable`
        builder.call(
            self.module
                .funcs
                .by_name("map_set")
                .expect("map_set not found"),
        );

        Ok(builder)
    }

    fn traverse_map_insert<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
        value: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Get the offset and length for this identifier in the literal memory
        let id_offset = *self
            .literal_memory_offet
            .get(name.as_str())
            .expect("map not found: {name}");
        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Create space on the call stack to write the key
        let ty = self
            .get_expr_type(key)
            .expect("map-set value expression must be typed")
            .clone();
        let (key_offset, key_size);
        (builder, key_offset, key_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the key to the data stack
        builder = self.traverse_expr(builder, key)?;

        // Write the key to the memory (it's already on the data stack)
        self.write_to_memory(builder.borrow_mut(), key_offset, 0, &ty);

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size);

        // Create space on the call stack to write the value
        let ty = self
            .get_expr_type(value)
            .expect("map-set value expression must be typed")
            .clone();
        let (val_offset, val_size);
        (builder, val_offset, val_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the value to the data stack
        builder = self.traverse_expr(builder, value)?;

        // Write the value to the memory (it's already on the data stack)
        self.write_to_memory(builder.borrow_mut(), val_offset, 0, &ty);

        // Push the value offset and size to the data stack
        builder.local_get(val_offset).i32_const(val_size);

        // Call the host interface function, `set_variable`
        builder.call(
            self.module
                .funcs
                .by_name("map_insert")
                .expect("map_insert not found"),
        );

        Ok(builder)
    }

    fn traverse_map_delete<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &SymbolicExpression,
        name: &ClarityName,
        key: &SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Get the offset and length for this identifier in the literal memory
        let id_offset = *self
            .literal_memory_offet
            .get(name.as_str())
            .expect("map not found: {name}");
        let id_length = name.len();

        // Push the identifier offset and length onto the data stack
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Create space on the call stack to write the key
        let ty = self
            .get_expr_type(key)
            .expect("map-set value expression must be typed")
            .clone();
        let (key_offset, key_size);
        (builder, key_offset, key_size) =
            self.create_call_stack_local(builder, self.stack_pointer, &ty);

        // Push the key to the data stack
        builder = self.traverse_expr(builder, key)?;

        // Write the key to the memory (it's already on the data stack)
        self.write_to_memory(builder.borrow_mut(), key_offset, 0, &ty);

        // Push the key offset and size to the data stack
        builder.local_get(key_offset).i32_const(key_size);

        // Call the host interface function, `set_variable`
        builder.call(
            self.module
                .funcs
                .by_name("map_delete")
                .expect("map_delete not found"),
        );

        Ok(builder)
    }
}

fn clar2wasm_ty(ty: &TypeSignature) -> Vec<ValType> {
    match ty {
        TypeSignature::NoType => vec![ValType::I32], // TODO: can this just be empty?
        TypeSignature::IntType => vec![ValType::I64, ValType::I64],
        TypeSignature::UIntType => vec![ValType::I64, ValType::I64],
        TypeSignature::ResponseType(inner_types) => {
            let mut types = vec![ValType::I32];
            types.extend(clar2wasm_ty(&inner_types.0));
            types.extend(clar2wasm_ty(&inner_types.1));
            types
        }
        TypeSignature::SequenceType(_) => vec![
            ValType::I32, // offset
            ValType::I32, // length
        ],
        TypeSignature::BoolType => vec![ValType::I32],
        TypeSignature::PrincipalType => vec![
            ValType::I32, // offset
            ValType::I32, // length
        ],
        TypeSignature::OptionalType(inner_ty) => {
            let mut types = vec![ValType::I32];
            types.extend(clar2wasm_ty(inner_ty));
            types
        }
        TypeSignature::TupleType(inner_types) => {
            let mut types = vec![];
            for inner_type in inner_types.get_type_map().values() {
                types.extend(clar2wasm_ty(inner_type));
            }
            types
        }
        _ => unimplemented!("{:?}", ty),
    }
}

/// Drop a value of type `ty` from the data stack.
fn drop_value(builder: &mut InstrSeqBuilder, ty: &TypeSignature) {
    let wasm_types = clar2wasm_ty(ty);
    (0..wasm_types.len()).for_each(|_| {
        builder.drop();
    });
}
