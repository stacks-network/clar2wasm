pub mod default_to;
pub mod equal;
pub mod optional;
pub mod regression;
pub mod response;
pub mod values;

use clarity::vm::types::{
    ASCIIData, BuffData, CharType, ListData, ListTypeData, OptionalData, ResponseData,
    SequenceData, SequenceSubtype, StringSubtype, StringUTF8Length, TupleData, TupleTypeSignature,
    TypeSignature, Value, MAX_VALUE_SIZE,
};
use proptest::prelude::*;

pub fn prop_signature() -> impl Strategy<Value = TypeSignature> {
    let leaf = prop_oneof![
        Just(TypeSignature::IntType),
        Just(TypeSignature::UIntType),
        Just(TypeSignature::BoolType),
        (0u32..128).prop_map(|s| TypeSignature::SequenceType(SequenceSubtype::BufferType(
            s.try_into().unwrap()
        ))),
        (0u32..128).prop_map(|s| TypeSignature::SequenceType(SequenceSubtype::StringType(
            StringSubtype::ASCII(s.try_into().unwrap())
        ))),
        // TODO: principal,
        // TODO: string-utf8
    ];
    leaf.prop_recursive(5, 64, 10, |inner| {
        prop_oneof![
            // optional type: 10% NoType + 90% any other type
            prop_oneof![
                1 => Just(TypeSignature::NoType),
                9 => inner.clone(),
            ]
            .prop_map(|t| TypeSignature::new_option(t).unwrap()),
            // response type: 20% (NoType, any) + 20% (any, NoType) + 60% (any, any)
            prop_oneof![
                1 => inner.clone().prop_map(|ok_ty| TypeSignature::new_response(ok_ty, TypeSignature::NoType).unwrap()),
                1 => inner.clone().prop_map(|err_ty| TypeSignature::new_response(TypeSignature::NoType, err_ty).unwrap()),
                3 => (inner.clone(), inner.clone()).prop_map(|(ok_ty, err_ty)| TypeSignature::new_response(ok_ty, err_ty).unwrap()),
            ],
            // tuple type
            prop::collection::btree_map(
                r#"[a-zA-Z]{1,16}"#.prop_map(|name| name.try_into().unwrap()),
                inner.clone(),
                1..8
            )
            .prop_map(|btree| TypeSignature::TupleType(btree.try_into().unwrap())),
            // list type
            (8u32..32, inner.clone()).prop_map(|(s, ty)| (ListTypeData::new_list(ty, s).unwrap()).into()),
        ]
    })
}

pub struct PropValue(Value);

impl From<Value> for PropValue {
    fn from(value: Value) -> Self {
        PropValue(value)
    }
}

impl From<PropValue> for Value {
    fn from(value: PropValue) -> Self {
        value.0
    }
}

impl std::fmt::Debug for PropValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self, f)
    }
}

impl std::fmt::Display for PropValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Value::Sequence(SequenceData::String(clarity::vm::types::CharType::ASCII(
                ASCIIData { data },
            ))) => {
                write!(f, "\"")?;
                for b in data {
                    if [b'\\', b'"'].contains(b) {
                        write!(f, "\\")?;
                    }
                    write!(f, "{}", *b as char)?;
                }
                write!(f, "\"")
            }
            Value::Optional(OptionalData { data }) => match data {
                Some(inner) => write!(f, "(some {})", PropValue(*inner.clone())),
                None => write!(f, "none"),
            },
            Value::Response(ResponseData { committed, data }) => {
                if *committed {
                    write!(f, "(ok {})", PropValue(*data.clone()))
                } else {
                    write!(f, "(err {})", PropValue(*data.clone()))
                }
            }
            Value::Sequence(SequenceData::List(ListData { data, .. })) => {
                write!(f, "(list")?;
                for d in data {
                    write!(f, " ")?;
                    write!(f, "{}", PropValue(d.clone()))?;
                }
                write!(f, ")")
            }
            Value::Tuple(data) => {
                write!(f, "(tuple")?;
                for (key, value) in &data.data_map {
                    write!(f, " ")?;
                    write!(f, "({} {})", &**key, PropValue(value.clone()))?;
                }
                write!(f, ")")
            }
            otherwise => write!(f, "{otherwise}"),
        }
    }
}

impl PropValue {
    pub fn any() -> impl Strategy<Value = Self> {
        prop_signature().prop_flat_map(prop_value).prop_map_into()
    }

    pub fn from_type(ty: TypeSignature) -> impl Strategy<Value = Self> {
        prop_value(ty).prop_map_into()
    }
}

fn prop_value(ty: TypeSignature) -> impl Strategy<Value = Value> {
    match ty {
        TypeSignature::NoType => unreachable!(),
        TypeSignature::IntType => int().boxed(),
        TypeSignature::UIntType => uint().boxed(),
        TypeSignature::BoolType => bool().boxed(),
        TypeSignature::OptionalType(ty) => optional(*ty).boxed(),
        TypeSignature::ResponseType(ok_err) => response(ok_err.0, ok_err.1).boxed(),
        TypeSignature::SequenceType(SequenceSubtype::BufferType(size)) => {
            buffer(size.into()).boxed()
        }
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(size))) => {
            string_ascii(size.into()).boxed()
        }
        TypeSignature::SequenceType(SequenceSubtype::ListType(list_type_data)) => {
            list(list_type_data).boxed()
        }
        TypeSignature::TupleType(tuple_ty) => tuple(tuple_ty).boxed(),

        // TODO
        TypeSignature::PrincipalType => todo!(),
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(_))) => todo!(),
        TypeSignature::CallableType(_) => todo!(),
        TypeSignature::ListUnionType(_) => todo!(),
        TypeSignature::TraitReferenceType(_) => todo!(),
    }
}

fn int() -> impl Strategy<Value = Value> {
    any::<i128>().prop_map(Value::Int)
}

fn uint() -> impl Strategy<Value = Value> {
    any::<u128>().prop_map(Value::UInt)
}

fn bool() -> impl Strategy<Value = Value> {
    any::<bool>().prop_map(Value::Bool)
}

pub fn string_ascii(size: u32) -> impl Strategy<Value = Value> {
    let size = size as usize;
    prop::collection::vec(0x20u8..0x7e, size..=size).prop_map(|bytes| {
        Value::Sequence(SequenceData::String(clarity::vm::types::CharType::ASCII(
            clarity::vm::types::ASCIIData { data: bytes },
        )))
    })
}

fn buffer(size: u32) -> impl Strategy<Value = Value> {
    let size = size as usize;
    prop::collection::vec(any::<u8>(), size..=size)
        .prop_map(|bytes| Value::Sequence(SequenceData::Buffer(BuffData { data: bytes })))
}

fn optional(inner_ty: TypeSignature) -> impl Strategy<Value = Value> {
    match inner_ty {
        TypeSignature::NoType => Just(Value::none()).boxed(),
        _ => prop::option::of(prop_value(inner_ty))
            .prop_map(|v| {
                Value::Optional(OptionalData {
                    data: v.map(Box::new),
                })
            })
            .boxed(),
    }
}

fn response(ok_ty: TypeSignature, err_ty: TypeSignature) -> impl Strategy<Value = Value> {
    match (ok_ty, err_ty) {
        (TypeSignature::NoType, err_ty) => prop_value(err_ty)
            .prop_map(|err| {
                Value::Response(ResponseData {
                    committed: false,
                    data: Box::new(err),
                })
            })
            .boxed(),
        (ok_ty, TypeSignature::NoType) => prop_value(ok_ty)
            .prop_map(|ok| {
                Value::Response(ResponseData {
                    committed: true,
                    data: Box::new(ok),
                })
            })
            .boxed(),
        (ok_ty, err_ty) => prop::result::maybe_err(prop_value(ok_ty), prop_value(err_ty))
            .prop_map(|res| {
                Value::Response(ResponseData {
                    committed: res.is_ok(),
                    data: res.map_or_else(Box::new, Box::new),
                })
            })
            .boxed(),
    }
}

fn list(list_type_data: ListTypeData) -> impl Strategy<Value = Value> {
    prop::collection::vec(
        prop_value(list_type_data.get_list_item_type().clone()),
        0..list_type_data.get_max_len() as usize,
    )
    .prop_map(move |v| {
        Value::Sequence(SequenceData::List(ListData {
            data: v,
            type_signature: list_type_data.clone(),
        }))
    })
}

fn tuple(tuple_ty: TupleTypeSignature) -> impl Strategy<Value = Value> {
    let fields: Vec<_> = tuple_ty.get_type_map().keys().cloned().collect();
    let strategies: Vec<_> = tuple_ty
        .get_type_map()
        .values()
        .cloned()
        .map(prop_value)
        .collect();
    strategies.prop_map(move |vec_values| {
        TupleData {
            type_signature: tuple_ty.clone(),
            data_map: fields.clone().into_iter().zip(vec_values).collect(),
        }
        .into()
    })
}

trait TypePrinter {
    fn type_string(&self) -> String;
}

impl TypePrinter for PropValue {
    fn type_string(&self) -> String {
        self.0.type_string()
    }
}

impl TypePrinter for Value {
    fn type_string(&self) -> String {
        match &self {
            Value::Int(_) => type_string(&TypeSignature::IntType),
            Value::UInt(_) => type_string(&TypeSignature::UIntType),
            Value::Bool(_) => type_string(&TypeSignature::BoolType),
            Value::Sequence(SequenceData::Buffer(length)) => type_string(
                &TypeSignature::SequenceType(SequenceSubtype::BufferType(length.len())),
            ),
            Value::Sequence(SequenceData::String(CharType::ASCII(data))) => {
                type_string(&TypeSignature::SequenceType(SequenceSubtype::StringType(
                    StringSubtype::ASCII(data.len()),
                )))
            }
            Value::Sequence(SequenceData::String(CharType::UTF8(data))) => type_string(
                &TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(
                    StringUTF8Length::try_from(u32::from(data.len()))
                        .unwrap_or(StringUTF8Length::try_from(MAX_VALUE_SIZE / 4).unwrap()),
                ))),
            ),
            Value::Optional(inner) => inner.type_string(),
            Value::Response(inner) => inner.type_string(),
            Value::Sequence(SequenceData::List(list_data)) => list_data.type_string(),
            Value::Tuple(data) => data.type_string(),
            Value::Principal(_) => type_string(&TypeSignature::PrincipalType),
            Value::CallableContract(_) => type_string(&TypeSignature::PrincipalType),
        }
    }
}

impl TypePrinter for OptionalData {
    fn type_string(&self) -> String {
        let inner = match self.data {
            Some(ref inner) => inner.type_string(),
            None => "int".to_owned(), // We need to default to something here
        };
        format!("(optional {inner})")
    }
}

impl TypePrinter for ResponseData {
    fn type_string(&self) -> String {
        let (ok_string, err_string) = if self.committed {
            (self.data.type_string(), "int".to_owned())
        } else {
            ("int".to_owned(), self.data.type_string())
        };
        format!("(response {} {})", ok_string, err_string)
    }
}

impl TypePrinter for ListData {
    fn type_string(&self) -> String {
        format!(
            "(list {} {})",
            self.data.len(),
            type_string(self.type_signature.get_list_item_type())
        )
    }
}

impl TypePrinter for TupleData {
    fn type_string(&self) -> String {
        type_string(&TypeSignature::TupleType(self.type_signature.clone()))
    }
}

fn type_string(ty: &TypeSignature) -> String {
    match ty {
        TypeSignature::IntType => "int".to_owned(),
        TypeSignature::UIntType => "uint".to_owned(),
        TypeSignature::BoolType => "bool".to_owned(),
        TypeSignature::OptionalType(inner) => format!("(optional {})", type_string(inner)),
        TypeSignature::ResponseType(inner) => format!(
            "(response {} {})",
            type_string(&inner.0),
            type_string(&inner.1)
        ),
        TypeSignature::SequenceType(SequenceSubtype::BufferType(len)) => format!("(buff {len})"),
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::ASCII(len))) => {
            format!("(string-ascii {len})")
        }
        TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(len))) => {
            format!("(string-utf8 {len})")
        }
        TypeSignature::SequenceType(SequenceSubtype::ListType(list_type_data)) => {
            format!(
                "(list {} {})",
                list_type_data.get_max_len(),
                type_string(list_type_data.get_list_item_type())
            )
        }
        TypeSignature::TupleType(tuple_ty) => {
            let mut s = String::new();
            s.push('{');
            for (key, value) in tuple_ty.get_type_map() {
                s.push_str(key);
                s.push(':');
                s.push_str(&type_string(value));
                s.push(',');
            }
            s.push('}');
            s
        }
        TypeSignature::PrincipalType => "principal".to_owned(),
        TypeSignature::CallableType(_) => "principal".to_owned(),
        TypeSignature::TraitReferenceType(_) => "principal".to_owned(),
        TypeSignature::NoType => "int".to_owned(), // Use "int" as a default type
        TypeSignature::ListUnionType(_) => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use clarity::vm::types::{PrincipalData, UTF8Data};

    use super::*;

    #[test]
    fn check_type_string() {
        assert_eq!(Value::Int(0).type_string(), "int");
        assert_eq!(Value::UInt(0).type_string(), "uint");
        assert_eq!(Value::Bool(false).type_string(), "bool");
        assert_eq!(
            Value::Sequence(SequenceData::Buffer(BuffData { data: vec![] })).type_string(),
            "(buff 0)"
        );
        assert_eq!(
            Value::Sequence(SequenceData::Buffer(BuffData {
                data: vec![1, 2, 3, 4, 5]
            }))
            .type_string(),
            "(buff 5)"
        );
        assert_eq!(
            Value::Sequence(SequenceData::String(CharType::ASCII(ASCIIData {
                data: vec![]
            })))
            .type_string(),
            "(string-ascii 0)"
        );
        assert_eq!(
            Value::Sequence(SequenceData::String(CharType::ASCII(ASCIIData {
                data: vec![0x68, 0x65, 0x6c, 0x6c, 0x6f]
            })))
            .type_string(),
            "(string-ascii 5)"
        );
        assert_eq!(
            Value::Sequence(SequenceData::String(CharType::UTF8(UTF8Data {
                data: vec![]
            })))
            .type_string(),
            "(string-utf8 0)"
        );
        assert_eq!(
            Value::Sequence(SequenceData::String(CharType::UTF8(UTF8Data {
                data: vec![vec![0x68], vec![0x65], vec![0x6c], vec![0x6c], vec![0x6f]]
            })))
            .type_string(),
            "(string-utf8 5)"
        );
        assert_eq!(
            Value::Optional(OptionalData { data: None }).type_string(),
            "(optional int)"
        );
        assert_eq!(
            Value::Optional(OptionalData {
                data: Some(Box::new(Value::UInt(0)))
            })
            .type_string(),
            "(optional uint)"
        );
        assert_eq!(
            Value::Response(ResponseData {
                committed: true,
                data: Box::new(Value::UInt(0))
            })
            .type_string(),
            "(response uint int)"
        );
        assert_eq!(
            Value::Response(ResponseData {
                committed: false,
                data: Box::new(Value::UInt(0))
            })
            .type_string(),
            "(response int uint)"
        );
        assert_eq!(
            Value::Sequence(SequenceData::List(ListData {
                data: vec![],
                type_signature: ListTypeData::new_list(TypeSignature::IntType, 0).unwrap()
            }))
            .type_string(),
            "(list 0 int)"
        );
        assert_eq!(
            Value::Sequence(SequenceData::List(ListData {
                data: vec![Value::Int(0), Value::Int(1), Value::Int(2)],
                type_signature: ListTypeData::new_list(TypeSignature::IntType, 3).unwrap()
            }))
            .type_string(),
            "(list 3 int)"
        );
        assert_eq!(
            Value::Tuple(
                TupleData::from_data(vec![
                    ("a".into(), Value::Int(42)),
                    ("b".into(), Value::UInt(42)),
                    ("c".into(), Value::Bool(true)),
                ])
                .unwrap()
            )
            .type_string(),
            "{a:int,b:uint,c:bool,}"
        );
        assert_eq!(
            Value::from(
                PrincipalData::parse_standard_principal(
                    "SM2J6ZY48GV1EZ5V2V5RB9MP66SW86PYKKQVX8X0G"
                )
                .unwrap()
            )
            .type_string(),
            "principal"
        );
        assert_eq!(
            // (list (ok 0))
            Value::cons_list_unsanitized(vec![Value::okay(Value::Int(0)).unwrap()])
                .unwrap()
                .type_string(),
            "(list 1 (response int int))"
        );
    }
}
