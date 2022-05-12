use std::ops::Deref;

use crate::error::{Error, Result};
use adapter::Adapter;
use bitcoin::hashes::Hash;
use bitcoin::util::bip32::ExtendedPubKey;
use bitcoin::{util::merkleblock::PartialMerkleTree, Transaction, Txid};
use checkpoint::CheckpointQueue;
use header_queue::HeaderQueue;
#[cfg(feature = "full")]
use orga::abci::InitChain;
use orga::call::Call;
use orga::client::Client;
use orga::coins::{Accounts, Address, Coin, Symbol};
use orga::collections::{
    map::{ChildMut, Ref},
    Deque, Map,
};
use orga::context::GetContext;
use orga::encoding::{Decode, Encode, Terminated};
#[cfg(feature = "full")]
use orga::plugins::{InitChainCtx, Validators};
use orga::plugins::{Time, Signer};
use orga::query::Query;
use orga::state::State;
use orga::{Error as OrgaError, Result as OrgaResult};
use signatory::SignatorySet;
use threshold_sig::{Pubkey, Signature, ThresholdSig};
use txid_set::{Outpoint, OutpointSet};

pub mod adapter;
pub mod checkpoint;
pub mod header_queue;
#[cfg(feature = "full")]
pub mod relayer;
pub mod signatory;
pub mod threshold_sig;
pub mod txid_set;

#[derive(State, Debug, Clone)]
pub struct Nbtc(());
impl Symbol for Nbtc {}

#[derive(State, Call, Query, Client)]
pub struct Bitcoin {
    pub headers: HeaderQueue,
    pub processed_outpoints: OutpointSet,
    pub checkpoints: CheckpointQueue,
    pub accounts: Accounts<Nbtc>,
    pub signatory_keys: Map<ConsensusKey, Xpub>,
}

pub type ConsensusKey = [u8; 32];

#[derive(Call, Query, Client)]
pub struct Xpub(ExtendedPubKey);

pub const XPUB_LENGTH: usize = 78;

impl Xpub {
    pub fn new(key: ExtendedPubKey) -> Self {
        Xpub(key)
    }
}

impl State for Xpub {
    type Encoding = Self;

    fn create(_: orga::store::Store, data: Self) -> OrgaResult<Self> {
        Ok(data)
    }

    fn flush(self) -> OrgaResult<Self> {
        Ok(self)
    }
}

impl Deref for Xpub {
    type Target = ExtendedPubKey;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Encode for Xpub {
    fn encode_into<W: std::io::Write>(&self, dest: &mut W) -> ed::Result<()> {
        let bytes = self.0.encode();
        dest.write_all(&bytes)?;
        Ok(())
    }

    fn encoding_length(&self) -> ed::Result<usize> {
        Ok(XPUB_LENGTH)
    }
}

impl Decode for Xpub {
    fn decode<R: std::io::Read>(mut input: R) -> ed::Result<Self> {
        let mut bytes = [0; XPUB_LENGTH];
        input.read_exact(&mut bytes)?;
        let key = ExtendedPubKey::decode(&bytes)
            .map_err(|_| ed::Error::UnexpectedByte(32))?;
        Ok(Xpub(key))
    }
}

impl Terminated for Xpub {}

impl Bitcoin {
    #[cfg(feature = "full")]
    #[call]
    pub fn set_signatory_key(&mut self, signatory_key: Xpub) -> Result<()> {
        let signer = self.context::<Signer>()
            .ok_or_else(|| Error::Orga(OrgaError::App("No Signer context available".into())))?
            .signer
            .ok_or_else(|| Error::Orga(OrgaError::App("Call must be signed".into())))?;

        let validators: &mut Validators = self.context().ok_or_else(|| {
            Error::Orga(orga::Error::App("No validator context found".to_string()))
        })?;

        let consensus_key = validators.consensus_key(signer)?.ok_or_else(|| {
            Error::Orga(orga::Error::App(
                "Signer does not have a consensus key".to_string(),
            ))
        })?;

        self.signatory_keys.insert(consensus_key, signatory_key)?;

        // TODO: rate-limiting

        Ok(())
    }

    #[call]
    pub fn relay_deposit(
        &mut self,
        btc_tx: Adapter<Transaction>,
        btc_height: u32,
        btc_proof: Adapter<PartialMerkleTree>,
        btc_vout: u32,
        sigset_index: u32,
        dest: Address,
    ) -> Result<()> {
        let btc_header = self
            .headers
            .get_by_height(btc_height)?
            .ok_or_else(|| OrgaError::App("Invalid bitcoin block height".to_string()))?;

        let mut txids = vec![];
        let mut block_indexes = vec![];
        let proof_merkle_root = btc_proof
            .extract_matches(&mut txids, &mut block_indexes)
            .map_err(|_| Error::BitcoinMerkleBlockError)?;
        if proof_merkle_root != btc_header.merkle_root() {
            return Err(OrgaError::App(
                "Bitcoin merkle proof does not match header".to_string(),
            ))?;
        }
        if txids.len() != 1 {
            return Err(OrgaError::App(
                "Bitcoin merkle proof contains an invalid number of txids".to_string(),
            ))?;
        }
        if txids[0] != btc_tx.txid() {
            return Err(OrgaError::App(
                "Bitcoin merkle proof does not match transaction".to_string(),
            ))?;
        }

        if btc_vout as usize >= btc_tx.output.len() {
            return Err(OrgaError::App("Output index is out of bounds".to_string()))?;
        }
        let output = &btc_tx.output[btc_vout as usize];

        let now = self
            .context::<Time>()
            .ok_or_else(|| Error::Orga(OrgaError::App("No time context available".to_string())))?
            .seconds as u64;
        let sigset = &self.checkpoints.get(sigset_index)?.sigset;
        if now > sigset.deposit_timeout() {
            return Err(OrgaError::App("Deposit timeout has expired".to_string()))?;
        }

        let expected_script = sigset.output_script(dest.bytes().to_vec());
        if output.script_pubkey != expected_script {
            return Err(OrgaError::App(
                "Output script does not match signature set".to_string(),
            ))?;
        }

        let outpoint = (btc_tx.txid().into_inner(), btc_vout);
        if self.processed_outpoints.contains(outpoint)? {
            return Err(OrgaError::App(
                "Output has already been relayed".to_string(),
            ))?;
        }

        self.processed_outpoints
            .insert(outpoint, sigset.deposit_timeout())?;

        // TODO: subtract deposit fee
        self.accounts.deposit(dest, Nbtc::mint(output.value))?;

        Ok(())
    }
}

#[cfg(feature = "full")]
impl InitChain for Bitcoin {
    fn init_chain(&mut self, ctx: &InitChainCtx) -> OrgaResult<()> {
        self.checkpoints.push_building()?;

        Ok(())
    }
}
