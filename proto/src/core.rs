pub mod v1 {
    #![allow(
        clippy::use_self,
        reason = "prost/pbjson-generated code names the concrete type instead of `Self`"
    )]

    crate::include_proto!("lyquor.core.v1");

    use lyquor_primitives::{
        Address as PrimitiveAddress, LyquidID as PrimitiveLyquidId, LyquidNumber as PrimitiveLyquidNumber,
        NodeID as PrimitiveNodeId,
    };
    use tonic::Status;

    fn parse_node_id(value: &str) -> Result<PrimitiveNodeId, Status> {
        value
            .parse()
            .map_err(|err| Status::invalid_argument(format!("invalid node_id `{value}`: {err:?}")))
    }

    fn parse_lyquid_id(value: &str) -> Result<PrimitiveLyquidId, Status> {
        value
            .parse()
            .map_err(|err| Status::invalid_argument(format!("invalid lyquid_id `{value}`: {err:?}")))
    }

    fn parse_address(value: &str) -> Result<PrimitiveAddress, Status> {
        value
            .parse()
            .map_err(|err| Status::invalid_argument(format!("invalid address `{value}`: {err:?}")))
    }

    impl From<PrimitiveNodeId> for NodeId {
        fn from(node_id: PrimitiveNodeId) -> Self {
            Self {
                value: node_id.to_string(),
            }
        }
    }

    impl From<&PrimitiveNodeId> for NodeId {
        fn from(node_id: &PrimitiveNodeId) -> Self {
            (*node_id).into()
        }
    }

    impl TryFrom<NodeId> for PrimitiveNodeId {
        type Error = Status;

        fn try_from(node_id: NodeId) -> Result<Self, Self::Error> {
            parse_node_id(&node_id.value)
        }
    }

    impl TryFrom<&NodeId> for PrimitiveNodeId {
        type Error = Status;

        fn try_from(node_id: &NodeId) -> Result<Self, Self::Error> {
            parse_node_id(&node_id.value)
        }
    }

    impl From<PrimitiveLyquidId> for LyquidId {
        fn from(lyquid_id: PrimitiveLyquidId) -> Self {
            Self {
                value: lyquid_id.to_string(),
            }
        }
    }

    impl From<&PrimitiveLyquidId> for LyquidId {
        fn from(lyquid_id: &PrimitiveLyquidId) -> Self {
            (*lyquid_id).into()
        }
    }

    impl TryFrom<LyquidId> for PrimitiveLyquidId {
        type Error = Status;

        fn try_from(lyquid_id: LyquidId) -> Result<Self, Self::Error> {
            parse_lyquid_id(&lyquid_id.value)
        }
    }

    impl TryFrom<&LyquidId> for PrimitiveLyquidId {
        type Error = Status;

        fn try_from(lyquid_id: &LyquidId) -> Result<Self, Self::Error> {
            parse_lyquid_id(&lyquid_id.value)
        }
    }

    impl From<PrimitiveLyquidNumber> for LyquidNumber {
        fn from(number: PrimitiveLyquidNumber) -> Self {
            Self {
                image: number.image,
                var: number.var,
            }
        }
    }

    impl From<&PrimitiveLyquidNumber> for LyquidNumber {
        fn from(number: &PrimitiveLyquidNumber) -> Self {
            (*number).into()
        }
    }

    impl From<LyquidNumber> for PrimitiveLyquidNumber {
        fn from(number: LyquidNumber) -> Self {
            Self {
                image: number.image,
                var: number.var,
            }
        }
    }

    impl From<&LyquidNumber> for PrimitiveLyquidNumber {
        fn from(number: &LyquidNumber) -> Self {
            (*number).into()
        }
    }

    impl From<PrimitiveAddress> for Address {
        fn from(address: PrimitiveAddress) -> Self {
            Self {
                value: address.to_string(),
            }
        }
    }

    impl From<&PrimitiveAddress> for Address {
        fn from(address: &PrimitiveAddress) -> Self {
            (*address).into()
        }
    }

    impl TryFrom<Address> for PrimitiveAddress {
        type Error = Status;

        fn try_from(address: Address) -> Result<Self, Self::Error> {
            parse_address(&address.value)
        }
    }

    impl TryFrom<&Address> for PrimitiveAddress {
        type Error = Status;

        fn try_from(address: &Address) -> Result<Self, Self::Error> {
            parse_address(&address.value)
        }
    }
}
