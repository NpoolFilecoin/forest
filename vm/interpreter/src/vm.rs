// Copyright 2019-2022 ChainSafe Systems
// SPDX-License-Identifier: Apache-2.0, MIT

use super::Rand;
use crate::fvm::{ForestExterns, ForestKernel, ForestMachine};
use actor::{cron, reward, system, AwardBlockRewardParams};
use address::Address;
use cid::Cid;
use clock::ChainEpoch;
use fil_types::BLOCK_GAS_LIMIT;
use fil_types::{
    verifier::{FullVerifier, ProofVerifier},
    DefaultNetworkParams, NetworkParams,
};
use forest_car::load_car;
use forest_encoding::Cbor;
use fvm::machine::NetworkConfig;
use fvm::machine::{Engine, Machine};
use fvm::trace::ExecutionTrace;
use fvm_ipld_encoding::RawBytes;
use fvm_shared::receipt::Receipt;
use fvm_shared::version::NetworkVersion;
use ipld_blockstore::BlockStore;
use ipld_blockstore::FvmStore;
use message::{ChainMessage, Message, MessageReceipt, UnsignedMessage};
use networks::UPGRADE_ACTORS_V4_HEIGHT;
use num_bigint::BigInt;
use num_traits::Zero;
use state_tree::StateTree;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use vm::{ActorError, ExitCode, Serialized, TokenAmount};

// const GAS_OVERUSE_NUM: i64 = 11;
// const GAS_OVERUSE_DENOM: i64 = 10;

/// Contains all messages to process through the VM as well as miner information for block rewards.
#[derive(Debug)]
pub struct BlockMessages {
    pub miner: Address,
    pub messages: Vec<ChainMessage>,
    pub win_count: i64,
}

/// Allows generation of the current circulating supply
/// given some context.
pub trait CircSupplyCalc: Clone + 'static {
    /// Retrieves total circulating supply on the network.
    fn get_supply<DB: BlockStore>(
        &self,
        height: ChainEpoch,
        state_tree: &StateTree<DB>,
    ) -> Result<TokenAmount, Box<dyn StdError>>;
    fn get_fil_vested<DB: BlockStore>(
        &self,
        height: ChainEpoch,
        store: &DB,
    ) -> Result<TokenAmount, Box<dyn StdError>>;
}

/// Trait to allow VM to retrieve state at an old epoch.
pub trait LookbackStateGetter<'db, DB> {
    /// Returns a state tree from the given epoch.
    fn state_lookback(&self, epoch: ChainEpoch) -> Result<StateTree<'db, DB>, Box<dyn StdError>>;
    fn chain_epoch_root(&self) -> Box<dyn Fn(ChainEpoch) -> Cid>;
}

/// Interpreter which handles execution of state transitioning messages and returns receipts
/// from the vm execution.
pub struct VM<
    'db,
    'r,
    DB: BlockStore + 'static,
    R,
    C: CircSupplyCalc,
    LB,
    V = FullVerifier,
    P = DefaultNetworkParams,
> {
    _state: Rc<RefCell<StateTree<'db, DB>>>,
    _store: &'db DB,
    _epoch: ChainEpoch,
    _rand: &'r R,
    _base_fee: BigInt,
    registered_actors: HashSet<Cid>,
    _network_version: NetworkVersion,
    _circ_supply_calc: C,
    fvm_executor: fvm::executor::DefaultExecutor<ForestKernel<DB>>,
    _lb_state: &'r LB,
    verifier: PhantomData<V>,
    params: PhantomData<P>,
}

pub fn import_actors(blockstore: &impl BlockStore) -> BTreeMap<NetworkVersion, Cid> {
    let bundles = [
        (NetworkVersion::V14, actors_v6::BUNDLE_CAR),
        (NetworkVersion::V15, actors_v7::BUNDLE_CAR),
    ];
    bundles
        .into_iter()
        .map(|(nv, car)| {
            let roots =
                async_std::task::block_on(async { load_car(blockstore, car).await.unwrap() });
            assert_eq!(roots.len(), 1);
            (nv, roots[0])
        })
        .collect()
}

impl<'db, 'r, DB, R, C, LB, V, P> VM<'db, 'r, DB, R, C, LB, V, P>
where
    DB: BlockStore,
    V: ProofVerifier,
    P: NetworkParams,
    R: Rand + Clone + 'static,
    C: CircSupplyCalc,
    LB: LookbackStateGetter<'db, DB>,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        root: Cid,
        store: &'db DB,
        store_arc: Arc<DB>,
        epoch: ChainEpoch,
        rand: &'r R,
        base_fee: BigInt,
        network_version: NetworkVersion,
        circ_supply_calc: C,
        override_circ_supply: Option<TokenAmount>,
        lb_state: &'r LB,
        engine: Engine,
    ) -> Result<Self, String> {
        let state = StateTree::new_from_root(store, &root).map_err(|e| e.to_string())?;
        let registered_actors = HashSet::new();
        let circ_supply = circ_supply_calc.get_supply(epoch, &state).unwrap();
        // let fil_vested = circ_supply_calc.get_fil_vested(epoch, store).unwrap();

        // Load the builtin actors bundles into the blockstore.
        let nv_actors = import_actors(store);

        // Get the builtin actors index for the concrete network version.
        let builtin_actors = *nv_actors
            .get(&network_version)
            .unwrap_or_else(|| panic!("no builtin actors index for nv {}", network_version));

        let mut context = NetworkConfig::new(network_version)
            .override_actors(builtin_actors)
            .for_epoch(epoch, root);
        context.set_base_fee(base_fee.clone());
        context.set_circulating_supply(circ_supply);
        context.enable_tracing();
        let fvm: fvm::machine::DefaultMachine<FvmStore<DB>, ForestExterns<DB>> =
            fvm::machine::DefaultMachine::new(
                &engine,
                &context,
                FvmStore::new(store_arc.clone()),
                ForestExterns::new(
                    rand.clone(),
                    epoch,
                    root,
                    lb_state.chain_epoch_root(),
                    store_arc,
                ),
            )
            .unwrap();
        let exec: fvm::executor::DefaultExecutor<ForestKernel<DB>> =
            fvm::executor::DefaultExecutor::new(ForestMachine {
                machine: fvm,
                circ_supply: override_circ_supply,
            });
        Ok(VM {
            _network_version: network_version,
            _state: Rc::new(RefCell::new(state)),
            _store: store,
            _epoch: epoch,
            _rand: rand,
            _base_fee: base_fee,
            registered_actors,
            fvm_executor: exec,
            _circ_supply_calc: circ_supply_calc,
            _lb_state: lb_state,
            verifier: PhantomData,
            params: PhantomData,
        })
    }

    /// Registers an actor that is not part of the set of default builtin actors by providing the
    /// code cid.
    pub fn register_actor(&mut self, code_cid: Cid) -> bool {
        self.registered_actors.insert(code_cid)
    }

    /// Gets registered actors that are not part of the set of default builtin actors.
    pub fn registered_actors(&self) -> &HashSet<Cid> {
        &self.registered_actors
    }

    /// Flush stores in VM and return state root.
    pub fn flush(&mut self) -> anyhow::Result<Cid> {
        self.fvm_executor.flush()
    }

    /// Get actor state from an address. Will be resolved to ID address.
    pub fn get_actor(&self, addr: &Address) -> Result<Option<vm::ActorState>, Box<dyn StdError>> {
        match self.fvm_executor.state_tree().get_actor(addr) {
            Ok(opt_state) => Ok(opt_state.map(vm::ActorState::from)),
            Err(err) => Err(format!("failed to get actor: {}", err).into()),
        }
    }

    pub fn run_cron(
        &mut self,
        epoch: ChainEpoch,
        callback: Option<&mut impl FnMut(&Cid, &ChainMessage, &ApplyRet) -> Result<(), String>>,
    ) -> Result<(), Box<dyn StdError>> {
        let cron_msg = UnsignedMessage {
            from: **system::ADDRESS,
            to: **cron::ADDRESS,
            // Epoch as sequence is intentional
            sequence: epoch as u64,
            // Arbitrarily large gas limit for cron (matching Lotus value)
            gas_limit: BLOCK_GAS_LIMIT * 10000,
            method_num: cron::Method::EpochTick as u64,
            params: Default::default(),
            value: Default::default(),
            version: Default::default(),
            gas_fee_cap: Default::default(),
            gas_premium: Default::default(),
        };

        let ret = self.apply_implicit_message(&cron_msg)?;
        if let Some(err) = ret.act_error {
            return Err(format!("failed to apply block cron message: {}", err).into());
        }

        if let Some(callback) = callback {
            callback(&(cron_msg.cid()?), &ChainMessage::Unsigned(cron_msg), &ret)?;
        }
        Ok(())
    }

    /// Flushes the StateTree and perform a state migration if there is a migration at this epoch.
    /// If there is no migration this function will return Ok(None).
    pub fn migrate_state(
        &self,
        epoch: ChainEpoch,
        _store: Arc<impl BlockStore + Send + Sync>,
    ) -> Result<Option<Cid>, Box<dyn StdError>> {
        match epoch {
            x if x == UPGRADE_ACTORS_V4_HEIGHT => {
                // FIXME: Support state migrations.
                panic!("Cannot migrate state when using FVM. See https://github.com/ChainSafe/forest/issues/1454 for updates.");
            }
            _ => Ok(None),
        }
    }

    /// Apply block messages from a Tipset.
    /// Returns the receipts from the transactions.
    pub fn apply_block_messages(
        &mut self,
        messages: &[BlockMessages],
        epoch: ChainEpoch,
        mut callback: Option<impl FnMut(&Cid, &ChainMessage, &ApplyRet) -> Result<(), String>>,
    ) -> Result<Vec<MessageReceipt>, Box<dyn StdError>> {
        let mut receipts = Vec::new();
        let mut processed = HashSet::<Cid>::default();

        for block in messages.iter() {
            let mut penalty = Default::default();
            let mut gas_reward = Default::default();

            let mut process_msg = |msg: &ChainMessage| -> Result<(), Box<dyn StdError>> {
                let cid = msg.cid()?;
                // Ensure no duplicate processing of a message
                if processed.contains(&cid) {
                    return Ok(());
                }
                let ret = self.apply_message(msg)?;

                if let Some(cb) = &mut callback {
                    cb(&cid, msg, &ret)?;
                }

                // Update totals
                gas_reward += &ret.miner_tip;
                penalty += &ret.penalty;
                receipts.push(ret.msg_receipt);

                // Add processed Cid to set of processed messages
                processed.insert(cid);
                Ok(())
            };

            for msg in block.messages.iter() {
                process_msg(msg)?;
            }

            // Generate reward transaction for the miner of the block
            let params = Serialized::serialize(AwardBlockRewardParams {
                miner: block.miner,
                penalty,
                gas_reward,
                win_count: block.win_count,
            })?;

            let rew_msg = UnsignedMessage {
                from: **system::ADDRESS,
                to: **reward::ADDRESS,
                method_num: reward::Method::AwardBlockReward as u64,
                params,
                // Epoch as sequence is intentional
                sequence: epoch as u64,
                gas_limit: 1 << 30,
                value: Default::default(),
                version: Default::default(),
                gas_fee_cap: Default::default(),
                gas_premium: Default::default(),
            };

            let ret = self.apply_implicit_message(&rew_msg)?;
            if let Some(err) = ret.act_error {
                return Err(format!(
                    "failed to apply reward message for miner {}: {}",
                    block.miner, err
                )
                .into());
            }

            // This is more of a sanity check, this should not be able to be hit.
            if ret.msg_receipt.exit_code != ExitCode::OK {
                return Err(format!(
                    "reward application message failed (exit: {:?})",
                    ret.msg_receipt.exit_code
                )
                .into());
            }

            if let Some(callback) = &mut callback {
                callback(&(rew_msg.cid()?), &ChainMessage::Unsigned(rew_msg), &ret)?;
            }
        }

        if let Err(e) = self.run_cron(epoch, callback.as_mut()) {
            log::error!("End of epoch cron failed to run: {}", e);
        }
        Ok(receipts)
    }

    /// Applies single message through vm and returns result from execution.
    pub fn apply_implicit_message(&mut self, msg: &UnsignedMessage) -> Result<ApplyRet, String> {
        self.apply_implicit_message_fvm(msg)
    }

    fn apply_implicit_message_fvm(&mut self, msg: &UnsignedMessage) -> Result<ApplyRet, String> {
        use fvm::executor::Executor;
        // raw_length is not used for Implicit messages.
        let raw_length = msg.marshal_cbor().expect("encoding error").len();
        let mut ret = self
            .fvm_executor
            .execute_message(msg.into(), fvm::executor::ApplyKind::Implicit, raw_length)
            .map_err(|e| format!("{:?}", e))?;
        ret.msg_receipt.gas_used = 0;
        ret.miner_tip = num_bigint::BigInt::zero();
        ret.penalty = num_bigint::BigInt::zero();
        Ok(ret.into())
    }

    /// Applies the state transition for a single message.
    /// Returns ApplyRet structure which contains the message receipt and some meta data.
    pub fn apply_message(&mut self, msg: &ChainMessage) -> Result<ApplyRet, String> {
        self.apply_message_fvm(msg)
    }

    fn apply_message_fvm(&mut self, msg: &ChainMessage) -> Result<ApplyRet, String> {
        check_message(msg.message())?;

        use fvm::executor::Executor;
        let unsigned = msg.message();
        let raw_length = msg.marshal_cbor().expect("encoding error").len();
        let fvm_ret = self
            .fvm_executor
            .execute_message(
                unsigned.into(),
                fvm::executor::ApplyKind::Explicit,
                raw_length,
            )
            .map_err(|e| format!("{:?}", e))?;
        Ok(fvm_ret.into())
    }
}

/// Apply message return data.
#[derive(Clone, Debug)]
pub struct ApplyRet {
    /// Message receipt for the transaction. This data is stored on chain.
    pub msg_receipt: MessageReceipt,
    /// Actor error from the transaction, if one exists.
    pub act_error: Option<ActorError>,
    /// Gas penalty from transaction, if any.
    pub penalty: BigInt,
    /// Tip given to miner from message.
    pub miner_tip: BigInt,

    // Gas stuffs
    pub base_fee_burn: TokenAmount,
    pub over_estimation_burn: TokenAmount,
    pub refund: TokenAmount,
    pub gas_refund: i64,
    pub gas_burned: i64,

    // /// Additional failure information for debugging, if any.
    // pub failure_info: Option<ApplyFailure>,
    /// Execution trace information, for debugging.
    pub exec_trace: ExecutionTrace,
}

impl Default for ApplyRet {
    fn default() -> ApplyRet {
        ApplyRet {
            msg_receipt: Receipt {
                exit_code: ExitCode::OK,
                return_data: RawBytes::default(),
                gas_used: 0,
            },
            act_error: None,
            penalty: BigInt::zero(),
            miner_tip: BigInt::zero(),
            base_fee_burn: TokenAmount::from(0),
            over_estimation_burn: TokenAmount::from(0),
            refund: TokenAmount::from(0),
            gas_refund: 0,
            gas_burned: 0,
            exec_trace: vec![],
        }
    }
}

impl PartialEq for ApplyRet {
    fn eq(&self, other: &Self) -> bool {
        self.penalty == other.penalty
            && self.miner_tip == other.miner_tip
            && self.act_error.is_some() == other.act_error.is_some()
            && self.msg_receipt.exit_code == other.msg_receipt.exit_code
            && self.msg_receipt.return_data == other.msg_receipt.return_data
            && self.msg_receipt.gas_used == other.msg_receipt.gas_used
    }
}

impl From<fvm::executor::ApplyRet> for ApplyRet {
    fn from(ret: fvm::executor::ApplyRet) -> Self {
        let fvm::executor::ApplyRet {
            msg_receipt,
            penalty,
            miner_tip,
            failure_info,
            base_fee_burn,
            over_estimation_burn,
            refund,
            gas_refund,
            gas_burned,
            exec_trace,
            ..
        } = ret;
        ApplyRet {
            msg_receipt,
            act_error: failure_info.map(ActorError::from),
            penalty,
            miner_tip,
            base_fee_burn,
            over_estimation_burn,
            refund,
            gas_refund,
            gas_burned,
            exec_trace,
        }
    }
}

/// Does some basic checks on the Message to see if the fields are valid.
fn check_message(msg: &UnsignedMessage) -> Result<(), &'static str> {
    if msg.gas_limit() == 0 {
        return Err("Message has no gas limit set");
    }
    if msg.gas_limit() < 0 {
        return Err("Message has negative gas limit");
    }

    Ok(())
}
