//
// Proptests that should only be executed
// when running Clarity::V2 or Clarity::v3.
//
#[cfg(not(feature = "test-clarity-v1"))]
mod clarity_v2_v3 {
    use clar2wasm::tools::{crosscheck, crosscheck_with_network, Network};
    use clarity::util::secp256k1::{Secp256k1PrivateKey, Secp256k1PublicKey};
    use clarity::vm::types::{
        ASCIIData, BuffData, CharType, OptionalData, PrincipalData, QualifiedContractIdentifier,
        SequenceData, StandardPrincipalData, TupleData,
    };
    use clarity::vm::Value;
    use proptest::prelude::{any, Just, Strategy};
    use proptest::{prop_oneof, proptest};

    use crate::{buffer, runtime_config, standard_principal, PropValue};

    fn strategies_for_version_byte() -> impl Strategy<Value = i32> {
        prop_oneof![
            25 => Just(0x1A),
            25 => Just(0x15),
            50 => 0x00..=0x1F
        ]
    }

    fn create_principal(version: u8, principal: &Vec<u8>, contract_name: Option<&str>) -> Value {
        if let Some(contract_name) = contract_name {
            // contract principal requested
            Value::Principal(PrincipalData::Contract(QualifiedContractIdentifier::new(
                StandardPrincipalData(
                    version,
                    principal
                        .as_slice()
                        .try_into()
                        .expect("slice bigger than 20 bytes"),
                ),
                contract_name.into(),
            )))
        } else {
            // standard principal requested
            Value::Principal(PrincipalData::Standard(StandardPrincipalData(
                version,
                principal
                    .as_slice()
                    .try_into()
                    .expect("slice bigger than 20 bytes"),
            )))
        }
    }

    fn create_error_construct(error_code: u8, principal_data: Option<Value>) -> Value {
        Value::error(
            TupleData::from_data(vec![
                ("error_code".into(), Value::UInt(error_code.into())),
                (
                    "value".into(),
                    Value::Optional(OptionalData {
                        data: principal_data.map(Box::new),
                    }),
                ),
            ])
            .unwrap()
            .into(),
        )
        .unwrap()
    }

    fn create_error_destruct(
        hash_bytes: Value,
        version_byte: u8,
        data: Option<Box<Value>>,
    ) -> Value {
        Value::error(
            TupleData::from_data(vec![
                ("hash-bytes".into(), hash_bytes),
                ("name".into(), Value::Optional(OptionalData { data })),
                (
                    "version".into(),
                    Value::Sequence(SequenceData::Buffer(BuffData {
                        data: vec![version_byte],
                    })),
                ),
            ])
            .unwrap()
            .into(),
        )
        .unwrap()
    }

    proptest! {
        #![proptest_config(runtime_config())]

        #[test]
        fn crosscheck_principal_construct(
            version_byte in strategies_for_version_byte(),
            hash_bytes in buffer(20),
            contract in "([a-zA-Z](([a-zA-Z0-9]|[-])){0, 30})".prop_flat_map(|name| {
                prop_oneof![Just(Some(name)), Just(None)]
            })
        ) {
            let expected_principal = create_principal(
                version_byte as u8,
                &hash_bytes.clone().expect_buff(20).unwrap(),
                contract.as_deref()
            );

            let expected = match version_byte {
                 // Since tests runs on a Testnet version,
                 // version_bytes single_sig (0x1A) || multi_sig (0x15), for Testnet,
                 // will return an Ok value.
                0x1A | 0x15 => Value::okay(expected_principal),
                0x00..=0x1F => Ok(create_error_construct(0, Some(expected_principal))),
                _ => Ok(create_error_construct(1, None))
            }.unwrap();

            let snippet = match contract {
                Some(ctc) => &format!("(principal-construct? 0x{:02X} {hash_bytes} \"{}\")", version_byte, ctc),
                None => &format!("(principal-construct? 0x{:02X} {hash_bytes})", version_byte)
            };

            crosscheck(
                snippet,
                Ok(Some(expected)),
            );
        }
    }

    proptest! {
        #![proptest_config(runtime_config())]

        #[test]
        fn crosscheck_principal_destruct(
            version_byte in strategies_for_version_byte(),
            hash_bytes in buffer(20),
            contract in "([a-zA-Z](([a-zA-Z0-9]|[-])){0, 30})".prop_flat_map(|name| {
                prop_oneof![Just(Some(name)), Just(None)]
            })
        ) {

            let expected_principal = create_principal(
                version_byte as u8,
                &hash_bytes.clone().expect_buff(20).unwrap(),
                contract.as_deref()
            );

            let data = contract.map(|ctc| Box::new(
                Value::Sequence(SequenceData::String(CharType::ASCII(ASCIIData {
                    data: ctc.into_bytes()
                })))
            ));

            let expected = match version_byte {
                // Since tests runs on a Testnet version,
                // version_bytes single_sig (0x1A) || multi_sig (0x15), for Testnet,
                // will return an Ok value.
                0x1A | 0x15 => Value::okay(
                    TupleData::from_data(vec![
                        ("hash-bytes".into(), hash_bytes),
                        ("name".into(), Value::Optional(OptionalData { data })),
                        (
                            "version".into(),
                            Value::Sequence(SequenceData::Buffer(BuffData {
                                data: vec![version_byte as u8],
                            })),
                        ),
                    ])
                    .unwrap()
                    .into()
                ),
                0x00..=0x1F => Ok(create_error_destruct(hash_bytes, version_byte as u8, data)),
                _ => Ok(create_error_destruct(hash_bytes, version_byte as u8, data)),
            }.unwrap();

            crosscheck(
                &format!("(principal-destruct? {})", PropValue::from(expected_principal.clone())),
                Ok(Some(expected)),
            );
        }
    }

    proptest! {
        #![proptest_config(runtime_config())]

        #[test]
        fn crosscheck_is_standard(
            principal in standard_principal().prop_map(PropValue::from)
        ) {
            let principal_str = principal.to_string();
            let expected_in_testnet = matches!(principal_str.get(0..3), Some("'ST") | Some("'SN"));
            let expected_in_mainnet = matches!(principal_str.get(0..3), Some("'SP") | Some("'SM"));

            let crosscheck_for = |network: &Network, expected: bool| {
                crosscheck_with_network(
                    network,
                    &format!("(is-standard {})", principal),
                    Ok(Some(Value::Bool(expected))),
                );
            };

            crosscheck_for(&Network::Mainnet, expected_in_mainnet);
            crosscheck_for(&Network::Testnet, expected_in_testnet);
        }
    }

    proptest! {
        #![proptest_config(runtime_config())]

        #[test]
        fn crosscheck_principal_of(
            seed in proptest::collection::vec(any::<u8>(), 20)
        ) {

            let private_key = Secp256k1PrivateKey::from_seed(&seed);
            let public_key = Secp256k1PublicKey::from_private(&private_key);

            let snippet = &format!("(is-standard (try! (principal-of? 0x{})))", public_key.to_hex());

            let crosscheck_for = |network: &Network, expected: bool| {
                crosscheck_with_network(
                    network,
                    snippet,
                    Ok(Some(Value::Bool(expected))),
                );
            };

            crosscheck_for(&Network::Mainnet, true);
            crosscheck_for(&Network::Testnet, true);
        }
    }
}
