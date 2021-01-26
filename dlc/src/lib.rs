//! # Rust DLC Library
//! Library for creating, signing and verifying transactions for the
//! Discreet Log Contract protocol.
//!

#![crate_name = "dlc"]
#![crate_type = "dylib"]
#![crate_type = "rlib"]
// Coding conventions
#![forbid(unsafe_code)]
#![deny(non_upper_case_globals)]
#![deny(non_camel_case_types)]
#![deny(non_snake_case)]
#![deny(unused_mut)]
#![deny(dead_code)]
#![deny(unused_imports)]
#![deny(missing_docs)]

extern crate bitcoin;
extern crate secp256k1;

use bitcoin::blockdata::{
    opcodes,
    script::{Builder, Script},
    transaction::{OutPoint, Transaction, TxIn, TxOut},
};
use secp256k1::ecdsa_adaptor::{AdaptorProof, AdaptorSignature};
use secp256k1::schnorrsig::{PublicKey as SchnorrPublicKey, Signature as SchnorrSignature};
use secp256k1::{Message, PublicKey, Secp256k1, SecretKey, Signature, Verification};

pub mod util;

/// Minimum value that can be included in a transaction output. Under this value,
/// outputs are discarded
/// See: https://github.com/discreetlogcontracts/dlcspecs/blob/master/Transactions.md#change-outputs
const DUST_LIMIT: u64 = 1000;

/// The transaction version
/// See: https://github.com/discreetlogcontracts/dlcspecs/blob/master/Transactions.md#funding-transaction
const TX_VERSION: i32 = 2;

/// The base weight of a fund transaction
/// See: https://github.com/discreetlogcontracts/dlcspecs/blob/master/Transactions.md#fees
const FUND_TX_BASE_WEIGHT: usize = 214;

/// The weight of a CET excluding payout outputs
/// See: https://github.com/discreetlogcontracts/dlcspecs/blob/master/Transactions.md#fees
const CET_BASE_WEIGHT: usize = 500;

/// The base weight of a transaction input computed as: (outpoint(36) + sequence(4) + scriptPubKeySize(1)) * 4
/// See: https://github.com/discreetlogcontracts/dlcspecs/blob/master/Transactions.md#fees
const TX_INPUT_BASE_WEIGHT: usize = 164;

/// The witness size of a P2WPKH input
/// See: https://github.com/discreetlogcontracts/dlcspecs/blob/master/Transactions.md#fees
pub const P2WPKH_WITNESS_SIZE: usize = 107;

// Setting the nSequence for every input of a transaction to this value disables
// both RBF and nLockTime usage.
const DISABLE_LOCKTIME: u32 = 0xffffffff;
// Setting the nSequence for every input of a transaction to this value disables
// RBF but enables nLockTime usage.
const ENABLE_LOCKTIME: u32 = 0xfffffffe;

/// Represents the payouts for a unique contract outcome. Offer party represents
/// the initiator of the contract while accept party represents the party
/// accepting the contract.
pub struct Payout {
    /// Payout for the offering party
    pub offer: u64,
    /// Payout for the accepting party
    pub accept: u64,
}

/// Contains the necessary transactions for establishing a DLC
pub struct DlcTransactions {
    /// The fund transaction locking both parties collaterals
    pub fund: Transaction,
    /// The contract execution transactions for closing the contract on a
    /// certain outcome
    pub cets: Vec<Transaction>,
    /// The refund transaction for returning the collateral for each party in
    /// case of an oracle misbehavior
    pub refund: Transaction,
}

/// Contains info about a utxo used for funding a DLC contract
pub struct TxInputInfo {
    /// The outpoint for the utxo
    pub outpoint: OutPoint,
    /// The maximum witness length
    pub max_witness_len: usize,
    /// The redeem script
    pub redeem_script: Script,
}

/// An error code.
#[derive(Copy, PartialEq, Eq, Clone, Debug)]
pub enum Error {
    /// Secp256k1 error
    Base(secp256k1::Error),
    /// An invalid argument was provided
    InvalidArgument,
}

impl From<secp256k1::Error> for Error {
    fn from(error: secp256k1::Error) -> Error {
        Error::Base(error)
    }
}

/// Contains the parameters required for creating DLC transactions for a single
/// party. Specifically these are the common fields between Offer and Accept
/// messages.
pub struct PartyParams {
    /// The public key for the fund multisig script
    pub fund_pubkey: PublicKey,
    /// An address to receive change
    pub change_script_pubkey: Script,
    /// An address to receive the outcome amount
    pub final_script_pubkey: Script,
    /// A list of inputs to fund the contract
    pub inputs: Vec<TxInputInfo>,
    /// The sum of the inputs values.
    pub input_amount: u64,
    /// The collateral put in the contract by the party
    pub collateral: u64,
}

impl PartyParams {
    /// Returns the change output for a single party as well as the fees that
    /// they are required to pay for the fund transaction and the cet or refund transaction.
    /// The change output value already accounts for the required fees.
    /// If input amount (sum of all input values) is lower than the sum of the collateral
    /// plus the required fees, an error is returned.
    pub fn get_change_output_and_fees(
        &self,
        fee_rate_per_vb: u64,
    ) -> Result<(TxOut, u64, u64), Error> {
        let inputs_weight: usize = self
            .inputs
            .iter()
            .map(|i| {
                let script_size = util::redeem_script_to_script_sig(&i.redeem_script).len();
                // Base tx input weight + redeem script weight (*4 because non witness)
                // + max witness weight (*1 because witness)
                TX_INPUT_BASE_WEIGHT + 4 * script_size + i.max_witness_len
            })
            .sum();

        // Value size + script length var_int + ouput script pubkey size
        let change_size = self.change_script_pubkey.len();
        // Change size is scaled by 4 from vBytes to weight units
        let change_weight = change_size * 4;

        // Base weight (nLocktime, nVersion, ...) is distributed among parties
        // independently of inputs contributed
        let this_party_fund_base_weight = FUND_TX_BASE_WEIGHT / 2;

        let total_fund_weight = this_party_fund_base_weight + inputs_weight + change_weight + 36;
        let fund_fee = util::weight_to_fee(total_fund_weight, fee_rate_per_vb);

        // Base weight (nLocktime, nVersion, funding input ...) is distributed
        // among parties independently of output types
        let this_party_cet_base_weight = CET_BASE_WEIGHT / 2;

        // size of the final script pubkey scaled by 4 from vBytes to weight units
        let output_spk_weight = self.final_script_pubkey.len() * 4;
        let total_cet_weight = this_party_cet_base_weight + output_spk_weight;
        let cet_or_refund_fee = util::weight_to_fee(total_cet_weight, fee_rate_per_vb);
        let required_input_funds = self.collateral + fund_fee + cet_or_refund_fee;
        if self.input_amount < required_input_funds {
            return Err(Error::InvalidArgument);
        }

        let change_output = TxOut {
            value: self.input_amount - required_input_funds,
            script_pubkey: self.change_script_pubkey.clone(),
        };

        Ok((change_output, fund_fee, cet_or_refund_fee))
    }

    fn get_unsigned_tx_inputs(&self, sequence: u32) -> Vec<TxIn> {
        self.inputs
            .iter()
            .map(|i| TxIn {
                previous_output: i.outpoint,
                script_sig: util::redeem_script_to_script_sig(&i.redeem_script),
                sequence,
                witness: Vec::new(),
            })
            .collect()
    }
}

/// Create the transactions for a DLC contract based on the provided parameters
pub fn create_dlc_transactions(
    offer_params: &PartyParams,
    accept_params: &PartyParams,
    payouts: &[Payout],
    refund_lock_time: u32,
    fee_rate_per_vb: u64,
    fund_lock_time: u32,
    cet_lock_time: u32,
) -> Result<DlcTransactions, Error> {
    let total_collateral = offer_params.collateral + accept_params.collateral;

    let has_proper_outcomes = payouts
        .iter()
        .all(|o| o.offer + o.accept == total_collateral);

    if !has_proper_outcomes {
        return Err(Error::InvalidArgument);
    }

    let (offer_change_output, offer_fund_fee, offer_cet_fee) =
        offer_params.get_change_output_and_fees(fee_rate_per_vb)?;
    let (accept_change_output, accept_fund_fee, accept_cet_fee) =
        accept_params.get_change_output_and_fees(fee_rate_per_vb)?;

    let fund_output_value = offer_params.input_amount + accept_params.input_amount
        - offer_change_output.value
        - accept_change_output.value
        - offer_fund_fee
        - accept_fund_fee;

    assert_eq!(
        total_collateral + offer_cet_fee + accept_cet_fee,
        fund_output_value
    );

    assert_eq!(
        offer_params.input_amount + accept_params.input_amount,
        fund_output_value
            + offer_change_output.value
            + accept_change_output.value
            + offer_fund_fee
            + accept_fund_fee
    );

    fn get_sequence(lock_time: u32) -> u32 {
        return if lock_time == 0 {
            DISABLE_LOCKTIME
        } else {
            ENABLE_LOCKTIME
        };
    }

    let fund_sequence = get_sequence(fund_lock_time);
    let offer_tx_ins = offer_params.get_unsigned_tx_inputs(fund_sequence);
    let accept_tx_ins = accept_params.get_unsigned_tx_inputs(fund_sequence);

    let fund_tx = create_funding_transaction(
        &offer_params.fund_pubkey,
        &accept_params.fund_pubkey,
        fund_output_value,
        &offer_tx_ins,
        &accept_tx_ins,
        offer_change_output,
        accept_change_output,
        fund_lock_time,
    );

    let fund_outpoint = OutPoint {
        txid: fund_tx.txid(),
        vout: 0,
    };

    let fund_tx_in = TxIn {
        previous_output: fund_outpoint,
        witness: Vec::new(),
        script_sig: Script::new(),
        sequence: get_sequence(cet_lock_time),
    };

    let cets = create_cets(
        &fund_tx_in,
        &offer_params.final_script_pubkey,
        &accept_params.final_script_pubkey,
        payouts,
        cet_lock_time,
    );

    let offer_refund_output = TxOut {
        value: offer_params.collateral,
        script_pubkey: offer_params.final_script_pubkey.clone(),
    };

    let accept_refund_ouput = TxOut {
        value: accept_params.collateral,
        script_pubkey: accept_params.final_script_pubkey.clone(),
    };

    let refund_tx = create_refund_transaction(
        offer_refund_output,
        accept_refund_ouput,
        fund_tx_in,
        refund_lock_time,
    );

    Ok(DlcTransactions {
        fund: fund_tx,
        cets,
        refund: refund_tx,
    })
}

/// Create a contract execution transaction
pub fn create_cet(
    offer_output: TxOut,
    accept_output: TxOut,
    fund_tx_in: &TxIn,
    lock_time: u32,
) -> Transaction {
    let output = [offer_output, accept_output]
        .iter()
        .filter(|o| o.value >= DUST_LIMIT)
        .cloned()
        .collect();

    Transaction {
        version: TX_VERSION,
        lock_time,
        input: vec![fund_tx_in.clone()],
        output,
    }
}

/// Create a set of contract execution transaction for each provided outcome
pub fn create_cets(
    fund_tx_input: &TxIn,
    offer_final_script_pubkey: &Script,
    accept_final_script_pubkey: &Script,
    outcomes: &[Payout],
    lock_time: u32,
) -> Vec<Transaction> {
    let mut txs: Vec<Transaction> = Vec::new();
    for i in 0..outcomes.len() {
        let offer_output = TxOut {
            value: outcomes[i].offer,
            script_pubkey: offer_final_script_pubkey.clone(),
        };
        let accept_output = TxOut {
            value: outcomes[i].accept,
            script_pubkey: accept_final_script_pubkey.clone(),
        };
        let tx = create_cet(offer_output, accept_output, fund_tx_input, lock_time);
        txs.push(tx);
    }

    txs
}

/// Create a funding transaction
pub fn create_funding_transaction(
    offer_fund_pubkey: &PublicKey,
    accept_fund_pubkey: &PublicKey,
    output_amount: u64,
    offer_inputs: &[TxIn],
    accept_inputs: &[TxIn],
    offer_change_output: TxOut,
    accept_change_output: TxOut,
    lock_time: u32,
) -> Transaction {
    let script = make_funding_redeemscript(&offer_fund_pubkey, &accept_fund_pubkey);

    let output: Vec<TxOut> = {
        let outs = vec![
            TxOut {
                value: output_amount,
                script_pubkey: script.to_v0_p2wsh(),
            },
            offer_change_output,
            accept_change_output,
        ];
        outs.into_iter().filter(|o| o.value >= DUST_LIMIT).collect()
    };

    let input = [&offer_inputs[..], &accept_inputs[..]].concat();

    let funding_transaction = Transaction {
        version: TX_VERSION,
        lock_time: lock_time,
        input,
        output,
    };

    return funding_transaction;
}

/// Create a refund transaction
pub fn create_refund_transaction(
    offer_output: TxOut,
    accept_output: TxOut,
    funding_input: TxIn,
    locktime: u32,
) -> Transaction {
    Transaction {
        version: TX_VERSION,
        lock_time: locktime,
        input: vec![funding_input],
        output: vec![offer_output, accept_output],
    }
}

/// Create the multisig redeem script for the funding output
pub fn make_funding_redeemscript(a: &PublicKey, b: &PublicKey) -> Script {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };

    Builder::new()
        .push_opcode(opcodes::all::OP_PUSHNUM_2)
        .push_slice(&first.serialize())
        .push_slice(&second.serialize())
        .push_opcode(opcodes::all::OP_PUSHNUM_2)
        .push_opcode(opcodes::all::OP_CHECKMULTISIG)
        .into_script()
}

/// Create an adaptor signature for the given cet
pub fn create_cet_adaptor_sig_from_point<C: secp256k1::Signing>(
    secp: &secp256k1::Secp256k1<C>,
    cet: &Transaction,
    adaptor_point: &PublicKey,
    funding_sk: &SecretKey,
    funding_script_pubkey: &Script,
    fund_output_value: u64,
) -> Result<(AdaptorSignature, AdaptorProof), Error> {
    let sig_hash = util::get_sig_hash_msg(cet, 0, funding_script_pubkey, fund_output_value);

    Ok(secp.adaptor_sign(&sig_hash, &funding_sk, adaptor_point))
}

/// Create an adaptor signature for the given cet
pub fn create_cet_adaptor_sig_from_oracle_info<C: secp256k1::Signing>(
    secp: &secp256k1::Secp256k1<C>,
    cet: &Transaction,
    oracle_pubkey: &SchnorrPublicKey,
    oracle_r_value: &SchnorrPublicKey,
    funding_sk: &SecretKey,
    funding_script_pubkey: &Script,
    fund_output_value: u64,
    msg: &Message,
) -> Result<(AdaptorSignature, AdaptorProof), Error> {
    let adaptor_point = secp.schnorrsig_compute_sig_point(msg, oracle_r_value, &oracle_pubkey)?;
    create_cet_adaptor_sig_from_point(
        secp,
        cet,
        &adaptor_point,
        funding_sk,
        funding_script_pubkey,
        fund_output_value,
    )
}

/// Crerate a set of adaptor signatures for the given cet/message pairs.
pub fn create_cet_adaptor_sigs_from_points<C: secp256k1::Signing>(
    secp: &secp256k1::Secp256k1<C>,
    inputs: &Vec<(&Transaction, &PublicKey)>,
    funding_sk: &SecretKey,
    funding_script_pubkey: &Script,
    fund_output_value: u64,
) -> Result<Vec<(AdaptorSignature, AdaptorProof)>, Error> {
    inputs
        .iter()
        .map(|(cet, adaptor_point)| {
            create_cet_adaptor_sig_from_point(
                secp,
                cet,
                adaptor_point,
                funding_sk,
                funding_script_pubkey,
                fund_output_value,
            )
        })
        .collect()
}

/// Crerate a set of adaptor signatures for the given cet/message pairs.
pub fn create_cet_adaptor_sigs_from_oracle_info<C: secp256k1::Signing>(
    secp: &secp256k1::Secp256k1<C>,
    inputs: &Vec<(&Transaction, &Message)>,
    oracle_pubkey: &SchnorrPublicKey,
    oracle_r_value: &SchnorrPublicKey,
    funding_sk: &SecretKey,
    funding_script_pubkey: &Script,
    fund_output_value: u64,
) -> Result<Vec<(AdaptorSignature, AdaptorProof)>, Error> {
    inputs
        .iter()
        .map(|(cet, msg)| {
            create_cet_adaptor_sig_from_oracle_info(
                secp,
                cet,
                oracle_pubkey,
                oracle_r_value,
                funding_sk,
                funding_script_pubkey,
                fund_output_value,
                msg,
            )
        })
        .collect()
}

/// Sign the given CET using own private key, adapt the counter party signature
/// and place both signatures and the funding multi sig script pubkey on the
/// witness stack
pub fn sign_cet<C: secp256k1::Signing>(
    secp: &secp256k1::Secp256k1<C>,
    cet: &mut Transaction,
    adaptor_signature: &AdaptorSignature,
    oracle_signature: &SchnorrSignature,
    funding_sk: &SecretKey,
    other_pk: &PublicKey,
    funding_script_pubkey: &Script,
    fund_output: u64,
) -> Result<(), Error> {
    let (_, adaptor_secret) = oracle_signature.decompose()?;

    let adapted_sig =
        secp.adaptor_adapt(&SecretKey::from_slice(adaptor_secret)?, adaptor_signature);

    util::sign_multi_sig_input(
        secp,
        cet,
        &adapted_sig,
        other_pk,
        funding_sk,
        funding_script_pubkey,
        fund_output,
        0,
    );

    Ok(())
}

/// Verify that a given adaptor signature for a given cet is valid with respect
/// to an adaptor point.
pub fn verify_cet_adaptor_sig_from_point(
    secp: &Secp256k1<secp256k1::All>,
    adaptor_sig: &AdaptorSignature,
    adaptor_proof: &AdaptorProof,
    cet: &Transaction,
    adaptor_point: &PublicKey,
    pubkey: &PublicKey,
    funding_script_pubkey: &Script,
    total_collateral: u64,
) -> Result<(), Error> {
    let sig_hash = util::get_sig_hash_msg(cet, 0, funding_script_pubkey, total_collateral);
    secp.adaptor_verify(
        &sig_hash,
        adaptor_sig,
        &pubkey,
        &adaptor_point,
        adaptor_proof,
    )?;
    Ok(())
}

/// Verify that a given adaptor signature for a given cet is valid with respect
/// to an oracle public key, nonce and a given message.
pub fn verify_cet_adaptor_sig_from_oracle_info(
    secp: &Secp256k1<secp256k1::All>,
    adaptor_sig: &AdaptorSignature,
    adaptor_proof: &AdaptorProof,
    cet: &Transaction,
    oracle_pubkey: &SchnorrPublicKey,
    oracle_r_value: &SchnorrPublicKey,
    pubkey: &PublicKey,
    funding_script_pubkey: &Script,
    total_collateral: u64,
    msg: &Message,
) -> Result<(), Error> {
    let adaptor_point = secp.schnorrsig_compute_sig_point(msg, oracle_r_value, &oracle_pubkey)?;
    verify_cet_adaptor_sig_from_point(
        secp,
        adaptor_sig,
        adaptor_proof,
        cet,
        &adaptor_point,
        pubkey,
        funding_script_pubkey,
        total_collateral,
    )
}

/// Verify a signature for a given transaction input.
pub fn verify_tx_input_sig<V: Verification>(
    secp: &Secp256k1<V>,
    signature: &Signature,
    tx: &Transaction,
    input_index: usize,
    script_pubkey: &Script,
    value: u64,
    pk: &PublicKey,
) -> Result<(), Error> {
    let sig_hash_msg = util::get_sig_hash_msg(tx, input_index, script_pubkey, value);
    secp.verify(&sig_hash_msg, signature, pk)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::blockdata::script::Script;
    use bitcoin::blockdata::transaction::{OutPoint, SigHashType};
    use bitcoin::consensus::encode::Encodable;
    use bitcoin::hashes::hex::FromHex;
    use bitcoin::{network::constants::Network, Address, Txid};
    use secp256k1::{
        rand::{Rng, RngCore},
        PublicKey, Secp256k1, SecretKey, Signing,
    };
    use std::fmt::Write;
    use std::str::FromStr;
    use util;

    fn create_txin_vec(sequence: u32) -> Vec<TxIn> {
        let mut inputs = Vec::new();
        let txin = TxIn {
            previous_output: OutPoint::default(),
            script_sig: Script::new(),
            sequence: sequence,
            witness: Vec::new(),
        };
        inputs.push(txin);
        inputs
    }

    fn create_multi_party_pub_keys() -> (PublicKey, PublicKey) {
        let secp = Secp256k1::new();
        let secret_key =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let pk = PublicKey::from_secret_key(&secp, &secret_key);
        let pk1 = pk;

        (pk, pk1)
    }

    fn create_test_tx_io() -> (TxOut, TxOut, TxIn) {
        let offer = TxOut {
            value: 1,
            script_pubkey: Script::new(),
        };

        let accept = TxOut {
            value: 2,
            script_pubkey: Script::new(),
        };

        let funding = TxIn {
            previous_output: OutPoint::default(),
            script_sig: Script::new(),
            sequence: 3,
            witness: Vec::new(),
        };

        (offer, accept, funding)
    }

    #[test]
    fn create_refund_transaction_test() {
        let (offer, accept, funding) = create_test_tx_io();

        let refund_transaction = create_refund_transaction(offer, accept, funding, 0);
        assert_eq!(2, refund_transaction.version);
        assert_eq!(0, refund_transaction.lock_time);
        assert_eq!(1, refund_transaction.output[0].value);
        assert_eq!(2, refund_transaction.output[1].value);
        assert_eq!(3, refund_transaction.input[0].sequence);
    }

    #[test]
    fn create_funding_transaction_test() {
        let (pk, pk1) = create_multi_party_pub_keys();

        let offer_inputs = create_txin_vec(0);
        let accept_inputs = create_txin_vec(1);

        let change = 1000;

        let total_collateral = 31415;

        let offer_change_output = TxOut {
            value: change,
            script_pubkey: Script::new(),
        };
        let accept_change_output = TxOut {
            value: change,
            script_pubkey: Script::new(),
        };

        let transaction = create_funding_transaction(
            &pk,
            &pk1,
            total_collateral,
            &offer_inputs,
            &accept_inputs,
            offer_change_output,
            accept_change_output,
            0,
        );

        assert_eq!(transaction.input[0].sequence, 0);
        assert_eq!(transaction.input[1].sequence, 1);

        assert_eq!(transaction.output[0].value, total_collateral);
        assert_eq!(transaction.output[1].value, change);
        assert_eq!(transaction.output[2].value, change);
        assert_eq!(transaction.output.len(), 3);
    }

    #[test]
    fn create_funding_transaction_with_outputs_less_than_dust_limit_test() {
        let (pk, pk1) = create_multi_party_pub_keys();

        let offer_inputs = create_txin_vec(0);
        let accept_inputs = create_txin_vec(1);

        let total_collateral = 31415;
        let change = 999;

        let offer_change_output = TxOut {
            value: change,
            script_pubkey: Script::new(),
        };
        let accept_change_output = TxOut {
            value: change,
            script_pubkey: Script::new(),
        };

        let transaction = create_funding_transaction(
            &pk,
            &pk1,
            total_collateral,
            &offer_inputs,
            &accept_inputs,
            offer_change_output,
            accept_change_output,
            0,
        );

        assert_eq!(transaction.output[0].value, total_collateral);
        assert_eq!(transaction.output.len(), 1);
    }

    #[test]
    fn create_funding_transaction_serialized_test() {
        let secp = Secp256k1::new();
        let input_amount = 5000000000;
        let change = 4899999719;
        let total_collateral = 200000312;
        let offer_change_address =
            Address::from_str("bcrt1qlgmznucxpdkp5k3ktsct7eh6qrc4tju7ktjukn").unwrap();
        let accept_change_address =
            Address::from_str("bcrt1qvh2dvgjctwh4z5w7sc93u7h4sug0yrdz2lgpqf").unwrap();

        let offer_change_output = TxOut {
            value: change,
            script_pubkey: offer_change_address.script_pubkey(),
        };

        let accept_change_output = TxOut {
            value: change,
            script_pubkey: accept_change_address.script_pubkey(),
        };

        let offer_input = TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(
                    "83266d6b22a9babf6ee469b88fd0d3a0c690525f7c903aff22ec8ee44214604f",
                )
                .unwrap(),
                vout: 0,
            },
            script_sig: Script::new(),
            sequence: 0xffffffff,
            witness: vec![Script::new().to_bytes()],
        };

        let accept_input = TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(
                    "bc92a22f07ef23c53af343397874b59f5f8c0eb37753af1d1a159a2177d4bb98",
                )
                .unwrap(),
                vout: 0,
            },
            script_sig: Script::new(),
            sequence: 0xffffffff,
            witness: vec![Script::new().to_bytes()],
        };
        let offer_fund_sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000001")
                .unwrap();
        let offer_fund_pubkey = PublicKey::from_secret_key(&secp, &offer_fund_sk);
        let accept_fund_sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000002")
                .unwrap();
        let accept_fund_pubkey = PublicKey::from_secret_key(&secp, &accept_fund_sk);
        let offer_input_sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000005")
                .unwrap();
        let accept_input_sk =
            SecretKey::from_str("0000000000000000000000000000000000000000000000000000000000000006")
                .unwrap();

        let expected_serialized = "020000000001024F601442E48EEC22FF3A907C5F5290C6A0D3D08FB869E46EBFBAA9226B6D26830000000000FFFFFFFF98BBD477219A151A1DAF5377B30E8C5F9FB574783943F33AC523EF072FA292BC0000000000FFFFFFFF0338C3EB0B000000002200209B984C7BAE3EFDDC3A3F0A20FF81BFE89ED1FE07FF13E562149EE654BED845DBE70F102401000000160014FA3629F3060B6C1A5A365C30BF66FA00F155CB9EE70F10240100000016001465D4D622585BAF5151DE860B1E7AF58710F20DA20247304402207108DE1563AE311F8D4217E1C0C7463386C1A135BE6AF88CBE8D89A3A08D65090220195A2B0140FB9BA83F20CF45AD6EA088BB0C6860C0D4995F1CF1353739CA65A90121022F8BDE4D1A07209355B4A7250A5C5128E88B84BDDC619AB7CBA8D569B240EFE4024730440220048716EAEE918AEBCB1BFCFAF7564E78293A7BB0164D9A7844E42FCEB5AE393C022022817D033C9DB19C5BDCADD49B7587A810B6FC2264158A59665ABA8AB298455B012103FFF97BD5755EEEA420453A14355235D382F6472F8568A18B2F057A146029755600000000";

        let mut fund_tx = create_funding_transaction(
            &offer_fund_pubkey,
            &accept_fund_pubkey,
            total_collateral,
            &[offer_input],
            &[accept_input],
            offer_change_output,
            accept_change_output,
            0,
        );

        util::sign_p2wpkh_input(
            &secp,
            &offer_input_sk,
            &mut fund_tx,
            0,
            SigHashType::All,
            input_amount,
        );

        util::sign_p2wpkh_input(
            &secp,
            &accept_input_sk,
            &mut fund_tx,
            1,
            SigHashType::All,
            input_amount,
        );

        let mut writer = Vec::new();
        fund_tx.consensus_encode(&mut writer).unwrap();
        let mut serialized = String::new();
        for x in writer {
            let _ = write!(&mut serialized, "{:02X}", x).unwrap();
        }

        assert_eq!(expected_serialized, serialized);
    }

    fn get_p2wpkh_script_pubkey<C: Signing, R: Rng + ?Sized>(
        secp: &Secp256k1<C>,
        rng: &mut R,
    ) -> Script {
        let sk = bitcoin::PrivateKey {
            key: SecretKey::new(rng),
            network: Network::Testnet,
            compressed: true,
        };
        let pk = bitcoin::PublicKey::from_private_key(&secp, &sk);
        Address::p2wpkh(&pk, Network::Testnet)
            .unwrap()
            .script_pubkey()
    }

    fn get_party_params(input_amount: u64, collateral: u64) -> (PartyParams, SecretKey) {
        let secp = Secp256k1::new();
        let mut rng = secp256k1::rand::thread_rng();
        let fund_privkey = SecretKey::new(&mut rng);
        (
            PartyParams {
                fund_pubkey: PublicKey::from_secret_key(&secp, &fund_privkey),
                change_script_pubkey: get_p2wpkh_script_pubkey(&secp, &mut rng),
                final_script_pubkey: get_p2wpkh_script_pubkey(&secp, &mut rng),
                input_amount,
                collateral,
                inputs: vec![TxInputInfo {
                    max_witness_len: 108,
                    redeem_script: Script::new(),
                    outpoint: OutPoint {
                        txid: Txid::from_hex(
                            "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456",
                        )
                        .unwrap(),
                        vout: 0,
                    },
                }],
            },
            fund_privkey,
        )
    }

    fn payouts() -> Vec<Payout> {
        vec![
            Payout {
                offer: 200000000,
                accept: 0,
            },
            Payout {
                offer: 0,
                accept: 200000000,
            },
        ]
    }

    #[test]
    fn get_change_output_and_fees_enough_funds() {
        // Arrange
        let (party_params, _) = get_party_params(100000, 10000);

        // Act

        let (change_out, fund_fee, cet_fee) = party_params.get_change_output_and_fees(4).unwrap();

        // Assert
        assert!(change_out.value > 0 && fund_fee > 0 && cet_fee > 0);
    }

    #[test]
    fn get_change_output_and_fees_not_enough_funds() {
        // Arrange
        let (party_params, _) = get_party_params(100000, 100000);

        // Act
        let res = party_params.get_change_output_and_fees(4);

        // Assert
        assert!(res.is_err());
    }

    #[test]
    fn create_dlc_transactions_no_error() {
        // Arrange
        let (offer_party_params, _) = get_party_params(1000000000, 100000000);
        let (accept_party_params, _) = get_party_params(1000000000, 100000000);

        // Act
        let dlc_txs = create_dlc_transactions(
            &offer_party_params,
            &accept_party_params,
            &payouts(),
            100,
            4,
            10,
            10,
        )
        .unwrap();

        // Assert
        assert_eq!(10, dlc_txs.fund.lock_time);
        assert_eq!(100, dlc_txs.refund.lock_time);
        assert!(dlc_txs.cets.iter().all(|x| x.lock_time == 10));
    }

    #[test]
    fn create_cet_adaptor_sig_is_valid() {
        // Arrange
        let secp = Secp256k1::new();
        let mut rng = secp256k1::rand::thread_rng();
        let (offer_party_params, offer_fund_sk) = get_party_params(1000000000, 100000000);
        let (accept_party_params, accept_fund_sk) = get_party_params(1000000000, 100000000);

        let dlc_txs = create_dlc_transactions(
            &offer_party_params,
            &accept_party_params,
            &payouts(),
            100,
            4,
            10,
            10,
        )
        .unwrap();

        let cets = dlc_txs.cets;
        let (oracle_kp, oracle_pubkey) = secp.generate_schnorrsig_keypair(&mut rng);
        let mut oracle_k_value = [0u8; 32];
        rng.fill_bytes(&mut oracle_k_value);
        let oracle_r_kp =
            secp256k1::schnorrsig::KeyPair::from_seckey_slice(&secp, &oracle_k_value).unwrap();
        let funding_script_pubkey = make_funding_redeemscript(
            &offer_party_params.fund_pubkey,
            &accept_party_params.fund_pubkey,
        );
        let oracle_r_value = SchnorrPublicKey::from_keypair(&secp, &oracle_r_kp);
        let messages: Vec<_> = vec![0, 1]
            .into_iter()
            .map(|x| Message::from_hashed_data::<secp256k1::bitcoin_hashes::sha256::Hash>(&[x]))
            .collect();
        let oracle_sig = secp.schnorrsig_sign_with_nonce(&messages[0], &oracle_kp, &oracle_k_value);
        let fund_output_value = dlc_txs.fund.output[0].value;

        // Act
        let cet_sigs = create_cet_adaptor_sigs_from_oracle_info(
            &secp,
            &cets.iter().zip(messages.iter()).collect(),
            &oracle_pubkey,
            &oracle_r_value,
            &offer_fund_sk,
            &funding_script_pubkey,
            fund_output_value,
        )
        .unwrap();

        let sign_res = sign_cet(
            &secp,
            &mut cets[0].clone(),
            &cet_sigs[0].0,
            &oracle_sig,
            &accept_fund_sk,
            &offer_party_params.fund_pubkey,
            &funding_script_pubkey,
            fund_output_value,
        );

        // Assert
        assert!(cet_sigs
            .iter()
            .enumerate()
            .all(|(i, x)| verify_cet_adaptor_sig_from_oracle_info(
                &secp,
                &x.0,
                &x.1,
                &cets[i],
                &oracle_pubkey,
                &oracle_r_value,
                &offer_party_params.fund_pubkey,
                &funding_script_pubkey,
                fund_output_value,
                &messages[i]
            )
            .is_ok()));
        assert!(sign_res.is_ok());
    }
}
