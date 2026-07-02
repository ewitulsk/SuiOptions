//! Layer 1 canonical cross-chain message types, shared by the off-chain signer
//! node and relayer (bridge-spec.md §2).
//!
//! The wire format and keccak256 digest here are byte-identical to the on-chain
//! `sui_bridge::message` (Move) and `Message.sol` (Solidity) implementations —
//! see `message::tests::known_digest_vector` for the three-way parity lock.

pub mod chain_id;
pub mod envelope;
pub mod message;
pub mod transfer;

pub use chain_id::ChainIdError;
pub use envelope::{Scheme, SignatureEnvelope};
pub use message::{keccak256, Bytes32, CrossChainMessage, VERSION};
pub use transfer::{TransferPayload, WIRE_DECIMALS};
