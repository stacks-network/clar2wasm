use std::collections::HashMap;

use clarity::vm::{
    analysis::ContractAnalysis,
    diagnostic::DiagnosableError,
    functions::NativeFunctions,
    types::{CharType, FunctionType, SequenceData, SequenceSubtype, StringSubtype, TypeSignature},
    SymbolicExpression,
};
use walrus::{
    ir::{BinaryOp, Block, InstrSeqType, LoadKind, MemArg, StoreKind},
    ActiveData, DataKind, FunctionBuilder, FunctionId, GlobalId, InstrSeqBuilder, LocalId, Module,
    ValType,
};

use crate::ast_visitor::{traverse, ASTVisitor};

/// WasmGenerator is a Clarity AST visitor that generates a WebAssembly module
/// as it traverses the AST.
pub struct WasmGenerator {
    /// The contract analysis, which contains the expressions and type
    /// information for the contract.
    contract_analysis: ContractAnalysis,
    /// The WebAssembly module that is being generated.
    module: Module,
    /// The error that occurred during generation, if any.
    error: Option<GeneratorError>,
    /// Offset of the end of the literal memory.
    literal_memory_end: u32,
    /// Global ID of the stack pointer.
    stack_pointer: GlobalId,
    /// Map identifier names to identifier numbers.
    identifiers: HashMap<String, i32>,
    /// Next identifier, used for contract constants, variables, and maps.
    next_identifier: i32,

    /// The locals for the current function.
    locals: HashMap<String, LocalId>,
    /// Size of the current function's stack.
    stack_size: i32,
}

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

impl WasmGenerator {
    pub fn new(contract_analysis: ContractAnalysis) -> WasmGenerator {
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
            locals: HashMap::new(),
            stack_size: 0,
            identifiers: HashMap::new(),
            next_identifier: 0,
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

        // Set the stack-pointer global at the end of the top-level function to
        // start just after the literals in memory.
        current_function
            .func_body()
            .i32_const(self.literal_memory_end as i32)
            .global_set(self.stack_pointer);

        // Insert a return instruction at the end of the top-level function so
        // that the top level always has no return value.
        current_function.func_body().return_();
        let top_level = current_function.finish(vec![], &mut self.module.funcs);
        self.module.exports.add(".top-level", top_level);

        Ok(self.module)
    }

    fn traverse_define_function(
        &mut self,
        name: &clarity::vm::ClarityName,
        body: &SymbolicExpression,
        kind: FunctionKind,
    ) -> Option<FunctionId> {
        let opt_function_type = match kind {
            FunctionKind::Private => self.contract_analysis.get_private_function(name.as_str()),
            FunctionKind::ReadOnly => self
                .contract_analysis
                .get_read_only_function_type(name.as_str()),
            FunctionKind::Public => self
                .contract_analysis
                .get_public_function_type(name.as_str()),
        };
        let function_type = if let Some(FunctionType::Fixed(fixed)) = opt_function_type {
            fixed
        } else {
            self.error = Some(GeneratorError::InternalError(match opt_function_type {
                Some(_) => "expected fixed function type".to_string(),
                None => format!("unable to find function type for {}", name.as_str()),
            }));
            return None;
        };

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
        // Store the initial stack offset.
        let initial_stack_pointer = self.module.locals.add(ValType::I32);
        func_body
            .global_get(self.stack_pointer)
            .local_set(initial_stack_pointer);

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
            .local_get(initial_stack_pointer)
            .global_set(self.stack_pointer);

        // Restore the top-level locals map.
        self.locals = top_level_locals;

        Some(func_builder.finish(param_locals, &mut self.module.funcs))
    }

    fn add_placeholder_for_type<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        ty: ValType,
    ) -> InstrSeqBuilder<'b> {
        match ty {
            ValType::I32 => builder.i32_const(0),
            ValType::I64 => builder.i64_const(0),
            ValType::F32 => builder.f32_const(0.0),
            ValType::F64 => builder.f64_const(0.0),
            ValType::V128 => unimplemented!("V128"),
            ValType::Externref => unimplemented!("Externref"),
            ValType::Funcref => unimplemented!("Funcref"),
        };
        builder
    }

    /// Gets the result type of the given `SymbolicExpression`.
    fn get_expr_type(&self, expr: &SymbolicExpression) -> &TypeSignature {
        self.contract_analysis
            .type_map
            .as_ref()
            .expect("type-checker must be called before Wasm generation")
            .get_type(expr)
            .expect("expression must be typed")
    }

    /// Adds a new string literal into the memory, and returns the offset and length.
    fn add_string_literal(&mut self, s: &CharType) -> (u32, u32) {
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
        (offset, len)
    }

    /// Adds a new string literal into the memory for an identifier
    fn add_identifier_string_literal(&mut self, name: &clarity::vm::ClarityName) -> (u32, u32) {
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
        (offset, len)
    }

    /// Push a new local onto the stack, adjusting the stack pointer and
    /// tracking the current function's stack size accordingly.
    pub fn create_stack_local<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        stack_pointer: GlobalId,
        // module: &mut Module,
        ty: &TypeSignature,
    ) -> (InstrSeqBuilder<'b>, LocalId, i32) {
        let size = match ty {
            TypeSignature::IntType | TypeSignature::UIntType => 16,
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(
                length,
            ))) => u32::from(length.clone()) as i32,
            _ => unimplemented!("Unsupported type for stack local"),
        };

        // Save the offset (current stack pointer) into a local
        let offset = self.module.locals.add(ValType::I32);
        builder.global_get(stack_pointer).local_tee(offset);

        // TODO: The total stack size can be computed at compile time, so we
        //       should be able to increment the stack pointer once in the function
        //       prelude with a constant instead of incrementing it for each local.
        // (global.set $stack-pointer (i32.add (global.get $stack-pointer) (i32.const <size>))
        builder
            .i32_const(size)
            .binop(BinaryOp::I32Add)
            .global_set(stack_pointer);
        self.stack_size += size;

        (builder, offset, size)
    }

    /// Write the value on the top of the stack, which has type `ty`, to the
    /// memory, at offset stored in local variable, `offset`.
    fn write_to_memory<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        offset: LocalId,
        ty: &TypeSignature,
    ) -> (InstrSeqBuilder<'b>, i32) {
        let memory = self.module.memories.iter().next().expect("no memory found");
        let size = match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Stack: TOP | Low | High | ...
                // Save the high/low to locals.
                let high = self.module.locals.add(ValType::I64);
                let low = self.module.locals.add(ValType::I64);
                builder.local_set(low).local_set(high);

                // Store the high/low to memory.
                builder.local_get(offset).local_get(high).store(
                    memory.id(),
                    StoreKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: 0,
                    },
                );
                builder.local_get(offset).local_get(low).store(
                    memory.id(),
                    StoreKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: 8,
                    },
                );
                16
            }
            _ => unimplemented!("Type not yet supported for writing to memory: {ty}"),
        };
        (builder, size)
    }

    /// Read a value from memory at offset stored in local variable `offset`,
    /// with type `ty`, and load it onto the top of the stack.
    fn read_from_memory<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        offset: LocalId,
        ty: &TypeSignature,
    ) -> (InstrSeqBuilder<'b>, i32) {
        let memory = self.module.memories.iter().next().expect("no memory found");
        let size = match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Memory: Offset -> | Low | High |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: 0,
                    },
                );
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: 8,
                    },
                );
                16
            }
            _ => unimplemented!("Type not yet supported for writing to memory: {ty}"),
        };
        (builder, size)
    }

    /// Return a unique identifier, used to identify a contract constant,
    /// variable or map.
    fn get_next_identifier(&mut self) -> i32 {
        let id = self.next_identifier;
        self.next_identifier += 1;
        id
    }
}

impl<'a> ASTVisitor<'a> for WasmGenerator {
    fn traverse_arithmetic<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &'a SymbolicExpression,
        func: NativeFunctions,
        operands: &'a [SymbolicExpression],
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let ty = self.get_expr_type(expr);
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

    fn visit_literal_value<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &'a SymbolicExpression,
        value: &clarity::vm::Value,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        match value {
            clarity::vm::Value::Int(i) => {
                builder.i64_const(((i >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const((i & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(builder)
            }
            clarity::vm::Value::UInt(u) => {
                builder.i64_const(((u >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const((u & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(builder)
            }
            clarity::vm::Value::Sequence(SequenceData::String(s)) => {
                let (offset, len) = self.add_string_literal(s);
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
        expr: &'a SymbolicExpression,
        atom: &'a clarity::vm::ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // FIXME: This should also handle constants and keywords
        let types = clar2wasm_ty(self.get_expr_type(expr));
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
        builder: InstrSeqBuilder<'b>,
        _expr: &'a SymbolicExpression,
        name: &'a clarity::vm::ClarityName,
        _parameters: Option<Vec<crate::ast_visitor::TypedVar<'a>>>,
        body: &'a SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if self
            .traverse_define_function(name, body, FunctionKind::Private)
            .is_some()
        {
            Ok(builder)
        } else {
            Err(builder)
        }
    }

    fn traverse_define_read_only<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &'a SymbolicExpression,
        name: &'a clarity::vm::ClarityName,
        _parameters: Option<Vec<crate::ast_visitor::TypedVar<'a>>>,
        body: &'a SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if let Some(function_id) = self.traverse_define_function(name, body, FunctionKind::ReadOnly)
        {
            self.module.exports.add(name.as_str(), function_id);
            Ok(builder)
        } else {
            Err(builder)
        }
    }

    fn traverse_define_public<'b>(
        &mut self,
        builder: InstrSeqBuilder<'b>,
        _expr: &'a SymbolicExpression,
        name: &'a clarity::vm::ClarityName,
        _parameters: Option<Vec<crate::ast_visitor::TypedVar<'a>>>,
        body: &'a SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        if let Some(function_id) = self.traverse_define_function(name, body, FunctionKind::Public) {
            self.module.exports.add(name.as_str(), function_id);
            Ok(builder)
        } else {
            Err(builder)
        }
    }

    fn traverse_define_data_var<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &'a SymbolicExpression,
        name: &'a clarity::vm::ClarityName,
        _data_type: &'a SymbolicExpression,
        initial: &'a SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        let var_id = self.get_next_identifier();
        self.identifiers.insert(name.to_string(), var_id);

        // Store the identifier as a string literal in the memory
        let (name_offset, name_length) = self.add_identifier_string_literal(name);

        // The initial value can be placed on the top of the memory, since at
        // the top-level, we have not set up the stack yet.
        let ty = self.get_expr_type(initial).clone();
        let offset = self.module.locals.add(ValType::I32);
        builder
            .i32_const(self.literal_memory_end as i32)
            .local_set(offset);

        // Traverse the initial value for the data variable (result is on stack)
        builder = self.traverse_expr(builder, initial)?;

        // Write the initial value to the memory, to be read by the host.
        let size;
        (builder, size) = self.write_to_memory(builder, offset, &ty);

        // Increment the literal memory end
        // FIXME: These initial values do not need to be saved in the literal
        //        memory forever... we just need them once, when .top-level
        //        is called.
        self.literal_memory_end += size as u32;

        // Push the variable identifier onto the stack
        builder.i32_const(var_id);

        // Push the name onto the stack
        builder
            .i32_const(name_offset as i32)
            .i32_const(name_length as i32);

        // Push the offset onto the stack
        builder.local_get(offset);

        // Push the size onto the stack
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

    fn traverse_ok<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &'a SymbolicExpression,
        value: &'a SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // (ok <val>) is represented by an i32 1, followed by the ok value,
        // followed by a placeholder for the err value
        builder.i32_const(1);
        builder = self.traverse_expr(builder, value)?;
        let ty = self.get_expr_type(expr);
        if let TypeSignature::ResponseType(inner_types) = ty {
            let err_types = clar2wasm_ty(&inner_types.1);
            for err_type in err_types.iter() {
                builder = self.add_placeholder_for_type(builder, *err_type);
            }
        } else {
            panic!("expected response type");
        }
        Ok(builder)
    }

    fn traverse_err<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        expr: &'a SymbolicExpression,
        value: &'a SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // (err <val>) is represented by an i32 0, followed by a placeholder
        // for the ok value, followed by the err value
        builder.i32_const(0);
        let ty = self.get_expr_type(expr);
        if let TypeSignature::ResponseType(inner_types) = ty {
            let ok_types = clar2wasm_ty(&inner_types.0);
            for ok_type in ok_types.iter() {
                builder = self.add_placeholder_for_type(builder, *ok_type);
            }
        } else {
            panic!("expected response type");
        }
        self.traverse_expr(builder, value)
    }

    fn visit_call_user_defined<'b>(
        &mut self,
        mut builder: InstrSeqBuilder<'b>,
        _expr: &'a SymbolicExpression,
        name: &'a clarity::vm::ClarityName,
        _args: &'a [SymbolicExpression],
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
        expr: &'a SymbolicExpression,
        lhs: &'a SymbolicExpression,
        rhs: &'a SymbolicExpression,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Create a new sequence to hold the result on the stack
        let ty = self.get_expr_type(expr).clone();
        let offset;
        (builder, offset, _) = self.create_stack_local(builder, self.stack_pointer, &ty);

        // Traverse the lhs, leaving it on the stack (offset, size)
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

        // Traverse the rhs, leaving it on the stack (offset, size)
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
        expr: &'a SymbolicExpression,
        name: &'a clarity::vm::ClarityName,
    ) -> Result<InstrSeqBuilder<'b>, InstrSeqBuilder<'b>> {
        // Get the identifier for this variable
        let var_id = *self
            .identifiers
            .get(&name.to_string())
            .expect("variable not found: {name}");

        // Create a new sequence to hold the result on the stack
        let ty = self.get_expr_type(expr).clone();
        let (offset, size);
        (builder, offset, size) = self.create_stack_local(builder, self.stack_pointer, &ty);

        // Push the variable identifier onto the stack
        builder.i32_const(var_id);

        // Push the offset and size to the stack
        builder.local_get(offset).i32_const(size);

        // Call the host interface function, `get_variable`
        builder.call(
            self.module
                .funcs
                .by_name("get_variable")
                .expect("function not found"),
        );

        // Host interface fills the result into the specified memory. Read it
        // back out, and place the value on the stack.
        (builder, _) = self.read_from_memory(builder, offset, &ty);

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
        TypeSignature::SequenceType(SequenceSubtype::StringType(_)) => vec![
            ValType::I32, // offset
            ValType::I32, // length
        ],
        _ => unimplemented!("{:?}", ty),
    }
}
