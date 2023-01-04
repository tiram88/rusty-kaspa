use crate::stubs::ScriptClass;

use super::{
    errors::{NonStandardError, NonStandardResult},
    Mempool,
};
use consensus_core::{
    constants::{MAX_SCRIPT_PUBLIC_KEY_VERSION, MAX_SOMPI},
    mass,
    tx::{MutableTransaction, TransactionOutput},
};

/// MAX_STANDARD_P2SH_SIG_OPS is the maximum number of signature operations
/// that are considered standard in a pay-to-script-hash script.
const MAX_STANDARD_P2SH_SIG_OPS: u8 = 15;

/// MAXIMUM_STANDARD_SIGNATURE_SCRIPT_SIZE is the maximum size allowed for a
/// transaction input signature script to be considered standard. This
/// value allows for a 15-of-15 CHECKMULTISIG pay-to-script-hash with
/// compressed keys.
///
/// The form of the overall script is: OP_0 <15 signatures> OP_PUSHDATA2
/// <2 bytes len> [OP_15 <15 pubkeys> OP_15 OP_CHECKMULTISIG]
///
/// For the p2sh script portion, each of the 15 compressed pubkeys are
/// 33 bytes (plus one for the OP_DATA_33 opcode), and the thus it totals
/// to (15*34)+3 = 513 bytes. Next, each of the 15 signatures is a max
/// of 73 bytes (plus one for the OP_DATA_73 opcode). Also, there is one
/// extra byte for the initial extra OP_0 push and 3 bytes for the
/// OP_PUSHDATA2 needed to specify the 513 bytes for the script push.
/// That brings the total to 1+(15*74)+3+513 = 1627. This value also
/// adds a few extra bytes to provide a little buffer.
/// (1 + 15*74 + 3) + (15*34 + 3) + 23 = 1650
const MAXIMUM_STANDARD_SIGNATURE_SCRIPT_SIZE: u64 = 1650;

/// MAXIMUM_STANDARD_TRANSACTION_MASS is the maximum mass allowed for transactions that
/// are considered standard and will therefore be relayed and considered for mining.
const MAXIMUM_STANDARD_TRANSACTION_MASS: u64 = 100_000;

impl Mempool {
    pub(crate) fn check_transaction_standard_in_isolation(&self, transaction: &MutableTransaction) -> NonStandardResult<()> {
        let transaction_id = transaction.id();

        // The transaction must be a currently supported version.
        //
        // This check is currently mirrored in consensus.
        // However, in a later version of Kaspa the consensus-valid transaction version range might diverge from the
        // standard transaction version range, and thus the validation should happen in both levels.
        if transaction.tx.version > self.config.maximum_standard_transaction_version
            || transaction.tx.version < self.config.minimum_standard_transaction_version
        {
            return Err(NonStandardError::RejectVersion(
                transaction_id,
                transaction.tx.version,
                self.config.minimum_standard_transaction_version,
                self.config.maximum_standard_transaction_version,
            ));
        }

        // Since extremely large transactions with a lot of inputs can cost
        // almost as much to process as the sender fees, limit the maximum
        // size of a transaction. This also helps mitigate CPU exhaustion
        // attacks.
        if transaction.calculated_mass.unwrap() > MAXIMUM_STANDARD_TRANSACTION_MASS {
            return Err(NonStandardError::RejectMass(
                transaction_id,
                transaction.calculated_mass.unwrap(),
                MAXIMUM_STANDARD_TRANSACTION_MASS,
            ));
        }

        for (i, input) in transaction.tx.inputs.iter().enumerate() {
            // Each transaction input signature script must not exceed the
            // maximum size allowed for a standard transaction.
            //
            // See the comment on MAXIMUM_STANDARD_SIGNATURE_SCRIPT_SIZE for
            // more details.
            let signature_script_len = input.signature_script.len() as u64;
            if signature_script_len > MAXIMUM_STANDARD_SIGNATURE_SCRIPT_SIZE {
                return Err(NonStandardError::RejectSignatureScriptSize(
                    transaction_id,
                    i,
                    signature_script_len,
                    MAXIMUM_STANDARD_SIGNATURE_SCRIPT_SIZE,
                ));
            }
        }

        // None of the output public key scripts can be a non-standard script or be "dust".
        for (i, output) in transaction.tx.outputs.iter().enumerate() {
            if output.script_public_key.version() > MAX_SCRIPT_PUBLIC_KEY_VERSION {
                return Err(NonStandardError::RejectScriptPublicKeyVersion(transaction_id, i));
            }

            // TODO: call script engine when available instead of this sanity check test
            // let script_class = txscript.get_script_class(output.script_public_key.script())
            let script_class =
                if output.script_public_key.script().len() < 34 { ScriptClass::NonStandard } else { ScriptClass::PubKey };
            if script_class == ScriptClass::NonStandard {
                return Err(NonStandardError::RejectOutputScriptClass(transaction_id, i));
            }

            if self.is_transaction_output_dust(output) {
                return Err(NonStandardError::RejectDust(transaction_id, i, output.value));
            }
        }

        Ok(())
    }

    /// is_transaction_output_dust returns whether or not the passed transaction output
    /// amount is considered dust or not based on the configured minimum transaction
    /// relay fee.
    ///
    /// Dust is defined in terms of the minimum transaction relay fee. In particular,
    /// if the cost to the network to spend coins is more than 1/3 of the minimum
    /// transaction relay fee, it is considered dust.
    ///
    /// It is exposed by [MiningManager] for use by transaction generators and wallets.
    pub(crate) fn is_transaction_output_dust(&self, transaction_output: &TransactionOutput) -> bool {
        // Unspendable outputs are considered dust.
        //
        // TODO: call script engine when available
        // if txscript.is_unspendable(transaction_output.script_public_key.script()) {
        //     return true
        // }
        // TODO: Remove this code when script engine is available
        if transaction_output.script_public_key.script().len() < 33 {
            return true;
        }

        // The total serialized size consists of the output and the associated
        // input script to redeem it. Since there is no input script
        // to redeem it yet, use the minimum size of a typical input script.
        //
        // Pay-to-pubkey bytes breakdown:
        //
        //  Output to pubkey (43 bytes):
        //   8 value, 1 script len, 34 script [1 OP_DATA_32,
        //   32 pubkey, 1 OP_CHECKSIG]
        //
        //  Input (105 bytes):
        //   36 prev outpoint, 1 script len, 64 script [1 OP_DATA_64,
        //   64 sig], 4 sequence
        //
        // The most common scripts are pay-to-pubkey, and as per the above
        // breakdown, the minimum size of a p2pk input script is 148 bytes. So
        // that figure is used.
        let total_serialized_size = mass::transaction_output_estimated_serialized_size(transaction_output) + 148;

        // The output is considered dust if the cost to the network to spend the
        // coins is more than 1/3 of the minimum free transaction relay fee.
        // mp.config.MinimumRelayTransactionFee is in sompi/KB, so multiply
        // by 1000 to convert to bytes.
        //
        // Using the typical values for a pay-to-pubkey transaction from
        // the breakdown above and the default minimum free transaction relay
        // fee of 1000, this equates to values less than 546 sompi being
        // considered dust.
        //
        // The following is equivalent to (value/total_serialized_size) * (1/3) * 1000
        // without needing to do floating point math.
        transaction_output.value as u128 * 1000 / (3 * total_serialized_size as u128)
            < self.config.minimum_relay_transaction_fee as u128
    }

    /// check_transaction_standard_in_context performs a series of checks on a transaction's
    /// inputs to ensure they are "standard". A standard transaction input within the
    /// context of this function is one whose referenced public key script is of a
    /// standard form and, for pay-to-script-hash, does not have more than
    /// maxStandardP2SHSigOps signature operations.
    /// In addition, makes sure that the transaction's fee is above the minimum for acceptance
    /// into the mempool and relay.
    pub(crate) fn check_transaction_standard_in_context(&self, transaction: &MutableTransaction) -> NonStandardResult<()> {
        let transaction_id = transaction.id();

        for (i, _input) in transaction.tx.inputs.iter().enumerate() {
            // It is safe to elide existence and index checks here since
            // they have already been checked prior to calling this
            // function.

            // TODO: call script engine when available
            // let entry = transaction.entries[i].as_ref().unwrap();
            // let origin_script_key = entry.script_public_key.script();
            // script_class = txscript.get_script_class(origin_script_key)
            let script_class = ScriptClass::ScriptHash;
            match script_class {
                ScriptClass::NonStandard => {
                    return Err(NonStandardError::RejectInputScriptClass(transaction_id, i));
                }

                // TODO: handle these 2 cases
                ScriptClass::PubKey => {}
                ScriptClass::_PubKeyECDSA => {}

                ScriptClass::ScriptHash => {
                    // TODO: call script engine when available
                    //  txscript.GetPreciseSigOpCount(input.SignatureScript, origin_script_key, true)
                    let num_sig_ops = 1;
                    if num_sig_ops > MAX_STANDARD_P2SH_SIG_OPS {
                        return Err(NonStandardError::RejectSignatureCount(transaction_id, i, num_sig_ops, MAX_STANDARD_P2SH_SIG_OPS));
                    }
                }
            }

            let minimum_fee = self.minimum_required_transaction_relay_fee(transaction.calculated_mass.unwrap());
            if transaction.calculated_fee.unwrap() < minimum_fee {
                return Err(NonStandardError::RejectInsufficientFee(transaction_id, transaction.calculated_fee.unwrap(), minimum_fee));
            }
        }

        Ok(())
    }

    /// minimum_required_transaction_relay_fee returns the minimum transaction fee required
    /// for a transaction with the passed mass to be accepted into the mempool and relayed.
    fn minimum_required_transaction_relay_fee(&self, mass: u64) -> u64 {
        // Calculate the minimum fee for a transaction to be allowed into the
        // mempool and relayed by scaling the base fee. MinimumRelayTransactionFee is in
        // sompi/kg so multiply by mass (which is in grams) and divide by 1000 to get
        // minimum sompis.
        let mut minimum_fee = (mass * self.config.minimum_relay_transaction_fee) / 1000;

        if minimum_fee == 0 {
            minimum_fee = self.config.minimum_relay_transaction_fee;
        }

        // Set the minimum fee to the maximum possible value if the calculated
        // fee is not in the valid range for monetary amounts.
        minimum_fee = minimum_fee.min(MAX_SOMPI);

        minimum_fee
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mempool::config::{Config, DEFAULT_MINIMUM_RELAY_TRANSACTION_FEE};
    use addresses::{Address, Prefix};
    use consensus_core::{
        api::ConsensusApi,
        block::{Block, BlockTemplate},
        blockstatus::BlockStatus,
        coinbase::MinerData,
        constants::{MAX_TX_IN_SEQUENCE_NUM, SOMPI_PER_KASPA, TX_VERSION},
        errors::{
            block::{BlockProcessResult, RuleError},
            coinbase::CoinbaseResult,
            tx::TxResult,
        },
        subnets::SUBNETWORK_ID_NATIVE,
        tx::{ScriptPublicKey, ScriptVec, Transaction, TransactionInput, TransactionOutpoint, TransactionOutput},
    };
    use futures_util::future::BoxFuture;
    use smallvec::smallvec;
    use std::sync::Arc;

    struct ConsensusMock {}

    impl ConsensusApi for ConsensusMock {
        fn build_block_template(self: Arc<Self>, _miner_data: MinerData, _txs: Vec<Transaction>) -> Result<BlockTemplate, RuleError> {
            todo!()
        }

        fn validate_and_insert_block(
            self: Arc<Self>,
            _block: Block,
            _update_virtual: bool,
        ) -> BoxFuture<'static, BlockProcessResult<BlockStatus>> {
            todo!()
        }

        fn validate_mempool_transaction_and_populate(self: Arc<Self>, _: &mut MutableTransaction) -> TxResult<()> {
            todo!()
        }

        fn calculate_transaction_mass(self: Arc<Self>, _: &Transaction) -> u64 {
            0
        }

        fn get_virtual_daa_score(self: Arc<Self>) -> u64 {
            0
        }

        fn modify_coinbase_payload(self: Arc<Self>, _: Vec<u8>, _: &MinerData) -> CoinbaseResult<Vec<u8>> {
            todo!()
        }
    }

    #[test]
    fn test_calc_min_required_tx_relay_fee() {
        struct TestScenario {
            name: &'static str,
            size: u64,
            minimum_relay_transaction_fee: u64,
            want: u64,
        }

        let tests = vec![
            TestScenario {
                // Ensure combination of size and fee that are less than 1000
                // produce a non-zero fee.
                name: "250 bytes with relay fee of 3",
                size: 250,
                minimum_relay_transaction_fee: 3,
                want: 3,
            },
            TestScenario {
                name: "100 bytes with default minimum relay fee",
                size: 100,
                minimum_relay_transaction_fee: DEFAULT_MINIMUM_RELAY_TRANSACTION_FEE,
                want: 100,
            },
            TestScenario {
                name: "max standard tx size with default minimum relay fee",
                size: MAXIMUM_STANDARD_TRANSACTION_MASS,
                minimum_relay_transaction_fee: DEFAULT_MINIMUM_RELAY_TRANSACTION_FEE,
                want: 100000,
            },
            TestScenario { name: "1500 bytes with 5000 relay fee", size: 1500, minimum_relay_transaction_fee: 5000, want: 7500 },
            TestScenario { name: "1500 bytes with 3000 relay fee", size: 1500, minimum_relay_transaction_fee: 3000, want: 4500 },
            TestScenario { name: "782 bytes with 5000 relay fee", size: 782, minimum_relay_transaction_fee: 5000, want: 3910 },
            TestScenario { name: "782 bytes with 3000 relay fee", size: 782, minimum_relay_transaction_fee: 3000, want: 2346 },
            TestScenario { name: "782 bytes with 2550 relay fee", size: 782, minimum_relay_transaction_fee: 2550, want: 1994 },
        ];

        for test in tests.iter() {
            // TODO: test all nets params
            let consensus = Arc::new(ConsensusMock {});
            let mut config = Config::build_default(1000, false, 500_000);
            config.minimum_relay_transaction_fee = test.minimum_relay_transaction_fee;
            let mempool = Mempool::with_config(consensus, config);

            let got = mempool.minimum_required_transaction_relay_fee(test.size);
            if got != test.want {
                println!("test_calc_min_required_tx_relay_fee test '{}' failed: got {}, want {}", test.name, got, test.want);
            }
            assert_eq!(test.want, got);
        }
    }

    #[test]
    fn test_is_transaction_output_dust() {
        let script_public_key = ScriptPublicKey::new(
            0,
            smallvec![
                0x76, 0xa9, 0x21, 0x03, 0x2f, 0x7e, 0x43, 0x0a, 0xa4, 0xc9, 0xd1, 0x59, 0x43, 0x7e, 0x84, 0xb9, 0x75, 0xdc, 0x76,
                0xd9, 0x00, 0x3b, 0xf0, 0x92, 0x2c, 0xf3, 0xaa, 0x45, 0x28, 0x46, 0x4b, 0xab, 0x78, 0x0d, 0xba, 0x5e
            ],
        );
        let invalid_script_public_key = ScriptPublicKey::new(0, smallvec![0x01]);

        struct TestScenario {
            name: &'static str,
            tx_out: TransactionOutput,
            minimum_relay_transaction_fee: u64,
            is_dust: bool,
        }

        let tests = vec![
            // Any value is allowed with a zero relay fee.
            TestScenario {
                name: "zero value with zero relay fee",
                tx_out: TransactionOutput::new(0, script_public_key.clone()),
                minimum_relay_transaction_fee: 0,
                is_dust: false,
            },
            // Zero value is dust with any relay fee"
            TestScenario {
                name: "zero value with very small tx fee",
                tx_out: TransactionOutput::new(0, script_public_key.clone()),
                minimum_relay_transaction_fee: 1,
                is_dust: true,
            },
            TestScenario {
                name: "36 byte public key script with value 605",
                tx_out: TransactionOutput::new(605, script_public_key.clone()),
                minimum_relay_transaction_fee: 1000,
                is_dust: true,
            },
            TestScenario {
                name: "36 byte public key script with value 606",
                tx_out: TransactionOutput::new(606, script_public_key.clone()),
                minimum_relay_transaction_fee: 1000,
                is_dust: false,
            },
            // Maximum allowed value is never dust.
            TestScenario {
                name: "max sompi amount is never dust",
                tx_out: TransactionOutput::new(MAX_SOMPI, script_public_key.clone()),
                minimum_relay_transaction_fee: 1000,
                is_dust: false,
            },
            // Maximum uint64 value causes no overflow.
            // Rust rewrite: this differs from golang version
            TestScenario {
                name: "maximum uint64 value",
                tx_out: TransactionOutput::new(u64::MAX, script_public_key),
                minimum_relay_transaction_fee: u64::MAX,
                is_dust: false,
            },
            // Unspendable script_public_key due to an invalid public key script.
            TestScenario {
                name: "unspendable script_public_key",
                tx_out: TransactionOutput::new(5000, invalid_script_public_key),
                minimum_relay_transaction_fee: 0,
                is_dust: true,
            },
        ];
        for test in tests {
            // TODO: test all nets params
            let consensus = Arc::new(ConsensusMock {});
            let mut config = Config::build_default(1000, false, 500_000);
            config.minimum_relay_transaction_fee = test.minimum_relay_transaction_fee;
            let mempool = Mempool::with_config(consensus, config);

            println!("test_is_transaction_output_dust test '{}' ", test.name);
            let res = mempool.is_transaction_output_dust(&test.tx_out);
            if res != test.is_dust {
                println!("test_is_transaction_output_dust test '{}' failed: got {}, want {}", test.name, res, test.is_dust);
            }
            assert_eq!(test.is_dust, res);
        }
    }

    #[test]
    fn test_check_transaction_standard_in_isolation() {
        // Create some dummy, but otherwise standard, data for transactions.
        let dummy_prev_out = TransactionOutpoint::new(hashes::Hash::from_u64_word(1), 1);
        let dummy_sig_script = vec![0u8; 65];
        let dummy_tx_input = TransactionInput::new(dummy_prev_out, dummy_sig_script, MAX_TX_IN_SEQUENCE_NUM, 1);
        let addr_hash = vec![1u8; 32];

        // TODO: call a constructor here when available
        let addr = Address { prefix: Prefix::Testnet, payload: addr_hash, version: 0u8 };

        // TODO: Replace this hack by a call to build the script (some txscript.PayToAddrScript(addr) equivalent).
        const ADDRESS_PUBLIC_KEY_SCRIPT_PUBLIC_KEY_VERSION: u16 = 0;
        const OP_CHECK_SIG: u8 = 172;
        let mut pay_to_pub_key_script = Vec::with_capacity(34);
        pay_to_pub_key_script.push(u8::try_from(addr.payload.len()).unwrap());
        pay_to_pub_key_script.extend(addr.payload);
        pay_to_pub_key_script.push(OP_CHECK_SIG);
        let script = ScriptVec::from_vec(pay_to_pub_key_script);
        let dummy_script_public_key = ScriptPublicKey::new(ADDRESS_PUBLIC_KEY_SCRIPT_PUBLIC_KEY_VERSION, script);

        let dummy_tx_out = TransactionOutput::new(SOMPI_PER_KASPA, dummy_script_public_key);

        struct TestScenario {
            name: &'static str,
            mtx: MutableTransaction,
            is_standard: bool,
        }

        fn new_mtx(tx: Transaction, mass: u64) -> MutableTransaction {
            let mut mtx = MutableTransaction::new(tx);
            mtx.calculated_mass = Some(mass);
            mtx
        }

        let tests = vec![
            TestScenario {
                name: "Typical pay-to-pubkey transaction",
                mtx: new_mtx(
                    Transaction::new(
                        TX_VERSION,
                        vec![dummy_tx_input.clone()],
                        vec![dummy_tx_out.clone()],
                        0,
                        SUBNETWORK_ID_NATIVE,
                        0,
                        vec![],
                    ),
                    1000,
                ),
                is_standard: true,
            },
            TestScenario {
                name: "Transaction version too high",
                mtx: new_mtx(
                    Transaction::new(
                        TX_VERSION + 1,
                        vec![dummy_tx_input.clone()],
                        vec![dummy_tx_out.clone()],
                        0,
                        SUBNETWORK_ID_NATIVE,
                        0,
                        vec![],
                    ),
                    1000,
                ),
                is_standard: false,
            },
            TestScenario {
                name: "Transaction size is too large",
                mtx: new_mtx(
                    Transaction::new(
                        TX_VERSION,
                        vec![dummy_tx_input.clone()],
                        vec![TransactionOutput::new(
                            0u64,
                            ScriptPublicKey::new(
                                ADDRESS_PUBLIC_KEY_SCRIPT_PUBLIC_KEY_VERSION,
                                ScriptVec::from_vec(vec![0u8; MAXIMUM_STANDARD_TRANSACTION_MASS as usize + 1]),
                            ),
                        )],
                        0,
                        SUBNETWORK_ID_NATIVE,
                        0,
                        vec![],
                    ),
                    1000,
                ),
                is_standard: false,
            },
            TestScenario {
                name: "Signature script size is too large",
                mtx: new_mtx(
                    Transaction::new(
                        TX_VERSION + 1,
                        vec![TransactionInput::new(
                            dummy_prev_out,
                            vec![0u8; MAXIMUM_STANDARD_SIGNATURE_SCRIPT_SIZE as usize + 1],
                            MAX_TX_IN_SEQUENCE_NUM,
                            1,
                        )],
                        vec![dummy_tx_out.clone()],
                        0,
                        SUBNETWORK_ID_NATIVE,
                        0,
                        vec![],
                    ),
                    1000,
                ),
                is_standard: false,
            },
            TestScenario {
                name: "Valid but non standard public key script",
                mtx: new_mtx(
                    Transaction::new(
                        TX_VERSION,
                        vec![dummy_tx_input.clone()],
                        vec![TransactionOutput::new(
                            SOMPI_PER_KASPA,
                            // TODO: build an invalid script ie. externalapi.ScriptPublicKey{[]byte{txscript.OpTrue}, 0}
                            // when txscript is available
                            ScriptPublicKey::new(ADDRESS_PUBLIC_KEY_SCRIPT_PUBLIC_KEY_VERSION, ScriptVec::from_vec(vec![81u8; 1])),
                        )],
                        0,
                        SUBNETWORK_ID_NATIVE,
                        0,
                        vec![],
                    ),
                    1000,
                ),
                is_standard: false,
            },
            TestScenario {
                name: "Dust output",
                mtx: new_mtx(
                    Transaction::new(
                        TX_VERSION,
                        vec![dummy_tx_input.clone()],
                        vec![TransactionOutput::new(0, dummy_tx_out.script_public_key)],
                        0,
                        SUBNETWORK_ID_NATIVE,
                        0,
                        vec![],
                    ),
                    1000,
                ),
                is_standard: false,
            },
            TestScenario {
                name: "Null-data transaction",
                mtx: new_mtx(
                    Transaction::new(
                        TX_VERSION,
                        vec![dummy_tx_input],
                        vec![TransactionOutput::new(
                            SOMPI_PER_KASPA,
                            // TODO: build an invalid script ie. externalapi.ScriptPublicKey{[]byte{txscript.OpReturn}, 0}
                            // when txscript is available
                            ScriptPublicKey::new(ADDRESS_PUBLIC_KEY_SCRIPT_PUBLIC_KEY_VERSION, ScriptVec::from_vec(vec![106u8; 1])),
                        )],
                        0,
                        SUBNETWORK_ID_NATIVE,
                        0,
                        vec![],
                    ),
                    1000,
                ),
                is_standard: false,
            },
        ];

        for test in tests {
            // TODO: test all nets params
            let consensus = Arc::new(ConsensusMock {});
            let config = Config::build_default(1000, false, 500_000);
            let mempool = Mempool::with_config(consensus, config);

            // Ensure standard-ness is as expected.
            println!("test_check_transaction_standard_in_isolation test '{}' ", test.name);
            let res = mempool.check_transaction_standard_in_isolation(&test.mtx);
            if res.is_ok() && test.is_standard {
                // Test passes since function returned standard for a
                // transaction which is intended to be standard.
                continue;
            }
            if res.is_ok() && !test.is_standard {
                println!("test_check_transaction_standard_in_isolation ({}): standard when it should not be", test.name);
            }
            if res.is_err() && test.is_standard {
                println!("test_check_transaction_standard_in_isolation ({}): nonstandard when it should not be: {:?}", test.name, res);
            }
            assert_eq!(res.is_ok(), test.is_standard);
        }
    }
}
