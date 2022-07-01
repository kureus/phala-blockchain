use crate::contracts;
use crate::system::{TransactionError, TransactionResult};
use anyhow::{anyhow, Result};
use parity_scale_codec::{Decode, Encode};
use phala_mq::{ContractClusterId, ContractId, MessageOrigin};
use pink::runtime::{BoxedEventCallbacks, ExecSideEffects};
use runtime::{AccountId, BlockNumber, Hash};
use sidevm::service::{Command as SidevmCommand, CommandSender, SystemMessage};

use super::contract_address_to_id;

#[derive(Debug, Encode, Decode)]
pub enum Command {
    InkMessage { nonce: Vec<u8>, message: Vec<u8> },
}

#[derive(Debug, Encode, Decode)]
pub enum Query {
    InkMessage(Vec<u8>),
    SidevmQuery(Vec<u8>),
}

#[derive(Debug, Encode, Decode)]
pub enum Response {
    Payload(Vec<u8>),
}

#[derive(Debug, Encode, Decode)]
pub enum QueryError {
    BadOrigin,
    RuntimeError(String),
    SidevmNotFound,
    NoResponse,
}

#[derive(Encode, Decode, Clone)]
pub struct Pink {
    instance: pink::Contract,
    cluster_id: ContractClusterId,
}

impl Pink {
    pub fn instantiate(
        cluster_id: ContractClusterId,
        storage: &mut pink::Storage,
        origin: AccountId,
        code_hash: Hash,
        input_data: Vec<u8>,
        salt: Vec<u8>,
        block_number: BlockNumber,
        now: u64,
        callbacks: Option<BoxedEventCallbacks>,
    ) -> Result<(Self, ExecSideEffects)> {
        let (instance, effects) = pink::Contract::new(
            storage,
            origin.clone(),
            code_hash,
            input_data,
            cluster_id.as_bytes().to_vec(),
            salt,
            block_number,
            now,
            callbacks,
        )
        .map_err(|err| anyhow!("Instantiate contract failed: {:?} origin={:?}", err, origin,))?;
        Ok((
            Self {
                cluster_id,
                instance,
            },
            effects,
        ))
    }

    pub fn from_address(address: AccountId, cluster_id: ContractClusterId) -> Self {
        let instance = pink::Contract::from_address(address);
        Self {
            instance,
            cluster_id,
        }
    }

    pub fn id(&self) -> ContractId {
        contract_address_to_id(&self.instance.address)
    }

    pub fn set_on_block_end_selector(&mut self, selector: u32) {
        self.instance.set_on_block_end_selector(selector)
    }
}

impl contracts::NativeContract for Pink {
    type Cmd = Command;

    type QReq = Query;

    type QResp = Result<Response, QueryError>;

    fn handle_query(
        &self,
        origin: Option<&AccountId>,
        req: Query,
        context: &mut contracts::QueryContext,
    ) -> Result<Response, QueryError> {
        match req {
            Query::InkMessage(input_data) => {
                let origin = origin.ok_or(QueryError::BadOrigin)?;
                let storage = &mut context.storage;

                let (ink_result, _effects) = self.instance.bare_call(
                    storage,
                    origin.clone(),
                    input_data,
                    true,
                    context.block_number,
                    context.now_ms,
                    ContractEventCallback::from_log_sender(
                        &context.log_sender,
                        context.block_number,
                    ),
                );
                if ink_result.result.is_err() {
                    log::error!("Pink [{:?}] query exec error: {:?}", self.id(), ink_result);
                }
                return Ok(Response::Payload(ink_result.encode()));
            }
            Query::SidevmQuery(payload) => {
                let handle = context
                    .sidevm_handle
                    .as_ref()
                    .ok_or(QueryError::SidevmNotFound)?;
                let cmd_sender = match handle {
                    contracts::SidevmHandle::Terminated(_) => {
                        return Err(QueryError::SidevmNotFound)
                    }
                    contracts::SidevmHandle::Running(sender) => sender,
                };
                let origin = origin.cloned().map(Into::into);

                let reply = tokio::task::block_in_place(move || {
                    tokio::runtime::Handle::current().block_on(async move {
                        let (reply_tx, rx) = tokio::sync::oneshot::channel();
                        let _x = cmd_sender
                            .send(SidevmCommand::PushQuery {
                                origin,
                                payload,
                                reply_tx,
                            })
                            .await;
                        rx.await
                    })
                });
                reply.or(Err(QueryError::NoResponse)).map(Response::Payload)
            }
        }
    }

    fn handle_command(
        &mut self,
        origin: MessageOrigin,
        cmd: Command,
        context: &mut contracts::NativeContext,
    ) -> TransactionResult {
        match cmd {
            Command::InkMessage { nonce: _, message } => {
                let origin: runtime::AccountId = match origin {
                    MessageOrigin::AccountId(origin) => origin.0.into(),
                    _ => return Err(TransactionError::BadOrigin),
                };

                let storage = cluster_storage(&mut context.contract_clusters, &self.cluster_id)
                    .expect("Pink cluster should always exists!");

                let (result, effects) = self.instance.bare_call(
                    storage,
                    origin.clone(),
                    message,
                    false,
                    context.block.block_number,
                    context.block.now_ms,
                    ContractEventCallback::from_log_sender(
                        &context.log_sender,
                        context.block.block_number,
                    ),
                );

                if let Some(log_sender) = &context.log_sender {
                    if let Err(_) = log_sender.try_send(SidevmCommand::PushSystemMessage(
                        SystemMessage::PinkMessageOutput {
                            origin: origin.clone().into(),
                            contract: self.instance.address.clone().into(),
                            block_number: context.block.block_number,
                            output: result.result.encode(),
                        },
                    )) {
                        error!("Pink emit message output to log receiver failed");
                    }
                }

                let _ = pink::transpose_contract_result(&result).map_err(|err| {
                    log::error!("Pink [{:?}] command exec error: {:?}", self.id(), err);
                    TransactionError::Other(format!("Call contract method failed: {:?}", err))
                })?;
                Ok(effects)
            }
        }
    }

    fn on_block_end(&mut self, context: &mut contracts::NativeContext) -> TransactionResult {
        let storage = cluster_storage(&mut context.contract_clusters, &self.cluster_id)
            .expect("Pink cluster should always exists!");
        let effects = self
            .instance
            .on_block_end(
                storage,
                context.block.block_number,
                context.block.now_ms,
                ContractEventCallback::from_log_sender(
                    &context.log_sender,
                    context.block.block_number,
                ),
            )
            .map_err(|err| {
                log::error!("Pink [{:?}] on_block_end exec error: {:?}", self.id(), err);
                TransactionError::Other(format!("Call contract on_block_end failed: {:?}", err))
            })?;
        Ok(effects)
    }

    fn snapshot(&self) -> Self {
        self.clone()
    }
}

fn cluster_storage<'a>(
    clusters: &'a mut cluster::ClusterKeeper,
    cluster_id: &ContractClusterId,
) -> Result<&'a mut pink::Storage> {
    clusters
        .get_cluster_storage_mut(cluster_id)
        .ok_or(anyhow!("Contract cluster {:?} not found! qed!", cluster_id))
}

pub mod cluster {
    use super::Pink;

    use anyhow::{Context, Result};
    use phala_crypto::sr25519::{Persistence, Sr25519SecretKey, KDF};
    use phala_mq::{ContractClusterId, ContractId};
    use phala_serde_more as more;
    use pink::{
        runtime::{BoxedEventCallbacks, ExecSideEffects},
        types::{AccountId, Hash},
    };
    use runtime::BlockNumber;
    use serde::{Deserialize, Serialize};
    use sp_core::sr25519;
    use sp_runtime::DispatchError;
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default, Serialize, Deserialize)]
    pub struct ClusterKeeper {
        clusters: BTreeMap<ContractClusterId, Cluster>,
    }

    impl ClusterKeeper {
        pub fn len(&self) -> usize {
            self.clusters.len()
        }

        pub fn instantiate_contract(
            &mut self,
            cluster_id: ContractClusterId,
            origin: AccountId,
            code_hash: Hash,
            input_data: Vec<u8>,
            salt: Vec<u8>,
            block_number: BlockNumber,
            now: u64,
            callbacks: Option<BoxedEventCallbacks>,
        ) -> Result<ExecSideEffects> {
            let cluster = self
                .get_cluster_mut(&cluster_id)
                .context("Cluster must exist before instantiation")?;
            let (_, effects) = Pink::instantiate(
                cluster_id,
                &mut cluster.storage,
                origin,
                code_hash,
                input_data,
                salt,
                block_number,
                now,
                callbacks,
            )?;
            Ok(effects)
        }

        pub fn get_cluster_storage_mut(
            &mut self,
            cluster_id: &ContractClusterId,
        ) -> Option<&mut pink::Storage> {
            Some(&mut self.clusters.get_mut(cluster_id)?.storage)
        }

        pub fn get_cluster_mut(&mut self, cluster_id: &ContractClusterId) -> Option<&mut Cluster> {
            self.clusters.get_mut(cluster_id)
        }

        pub fn get_cluster_or_default_mut(
            &mut self,
            cluster_id: &ContractClusterId,
            cluster_key: &sr25519::Pair,
        ) -> &mut Cluster {
            self.clusters.entry(cluster_id.clone()).or_insert_with(|| {
                let mut cluster = Cluster {
                    storage: Default::default(),
                    contracts: Default::default(),
                    key: cluster_key.clone(),
                    config: Default::default(),
                };
                let seed_key = cluster_key
                    .derive_sr25519_pair(&[b"ink key derivation seed"])
                    .expect("Derive key seed should always success!");
                cluster.set_id(cluster_id);
                cluster.set_key_seed(seed_key.dump_secret_key());
                cluster
            })
        }
    }

    #[derive(Serialize, Deserialize, Default)]
    pub struct ClusterConfig {
        pub log_receiver: Option<ContractId>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Cluster {
        pub storage: pink::Storage,
        contracts: BTreeSet<ContractId>,
        #[serde(with = "more::key_bytes")]
        key: sr25519::Pair,
        pub config: ClusterConfig,
    }

    impl Cluster {
        /// Add a new contract to the cluster. Returns true if the contract is new.
        pub fn add_contract(&mut self, address: ContractId) -> bool {
            self.contracts.insert(address)
        }

        pub fn key(&self) -> &sr25519::Pair {
            &self.key
        }

        pub fn set_id(&mut self, id: &ContractClusterId) {
            self.storage.set_cluster_id(id.as_bytes());
        }

        pub fn set_key_seed(&mut self, seed: Sr25519SecretKey) {
            self.storage.set_key_seed(seed);
        }

        pub fn upload_code(
            &mut self,
            origin: AccountId,
            code: Vec<u8>,
        ) -> Result<Hash, DispatchError> {
            self.storage.upload_code(origin, code)
        }
    }
}

pub(crate) struct ContractEventCallback {
    log_sender: CommandSender,
    block_number: BlockNumber,
}

impl ContractEventCallback {
    pub fn new(log_sender: CommandSender, block_number: BlockNumber) -> Self {
        ContractEventCallback {
            log_sender,
            block_number,
        }
    }

    pub fn from_log_sender(
        log_sender: &Option<CommandSender>,
        block_number: BlockNumber,
    ) -> Option<BoxedEventCallbacks> {
        Some(Box::new(ContractEventCallback::new(
            log_sender.as_ref().cloned()?,
            block_number,
        )))
    }
}

impl pink::runtime::EventCallbacks for ContractEventCallback {
    fn emit_log(&self, contract: &AccountId, in_query: bool, level: u8, message: String) {
        if let Err(_) =
            self.log_sender
                .try_send(SidevmCommand::PushSystemMessage(SystemMessage::PinkLog {
                    block_number: self.block_number,
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as _,
                    in_query,
                    contract: contract.clone().into(),
                    level,
                    message,
                }))
        {
            error!("Pink emit_log failed");
        }
    }
}
