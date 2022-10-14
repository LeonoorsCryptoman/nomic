use orga::call::Call;
use orga::client::Client;
use orga::coins::{Address, Amount};
use orga::collections::{ChildMut, Map};
use orga::context::GetContext;
#[cfg(feature = "full")]
use orga::migrate::Migrate;
use orga::plugins::{Paid, Signer};
use orga::prelude::Decimal;
use orga::query::Query;
use orga::state::State;
use orga::{Error, Result};

use super::app::Nom;

const MAX_STAKED: u64 = 1_000_000_000;
const AIRDROP_II_TOTAL: u64 = 3_500_000_000_000;

#[derive(State, Query, Call, Client)]
pub struct Airdrop {
    accounts: Map<Address, Account>,
}

impl Airdrop {
    #[query]
    pub fn get(&self, address: Address) -> Result<Option<Account>> {
        Ok(self.accounts.get(address)?.map(|a| a.clone()))
    }

    pub fn get_mut(&mut self, address: Address) -> Result<Option<ChildMut<Address, Account>>> {
        self.accounts.get_mut(address)
    }

    pub fn signer_acct_mut(&mut self) -> Result<ChildMut<Address, Account>> {
        let signer = self
            .context::<Signer>()
            .ok_or_else(|| Error::Signer("No Signer context available".into()))?
            .signer
            .ok_or_else(|| Error::Coins("Unauthorized account action".into()))?;

        self.accounts
            .get_mut(signer)?
            .ok_or_else(|| Error::Coins("No airdrop account for signer".into()))
    }

    fn pay_as_funding(&mut self, amount: u64) -> Result<()> {
        let paid = self
            .context::<Paid>()
            .ok_or_else(|| Error::Coins("No Paid context found".into()))?;

        paid.give::<Nom, _>(amount)
    }

    #[call]
    pub fn claim_airdrop1(&mut self) -> Result<()> {
        let mut acct = self.signer_acct_mut()?;
        let amount = acct.airdrop1.claim()?;
        self.pay_as_funding(amount)?;
        Ok(())
    }

    #[call]
    pub fn claim_btc_deposit(&mut self) -> Result<()> {
        let mut acct = self.signer_acct_mut()?;
        let amount = acct.btc_deposit.claim()?;
        self.pay_as_funding(amount)?;
        Ok(())
    }

    #[call]
    pub fn claim_btc_withdraw(&mut self) -> Result<()> {
        let mut acct = self.signer_acct_mut()?;
        let amount = acct.btc_withdraw.claim()?;
        self.pay_as_funding(amount)?;
        Ok(())
    }

    #[call]
    pub fn claim_ibc_transfer(&mut self) -> Result<()> {
        let mut acct = self.signer_acct_mut()?;
        let amount = acct.ibc_transfer.claim()?;
        self.pay_as_funding(amount)?;
        Ok(())
    }

    #[cfg(feature = "full")]
    pub fn init_from_airdrop2_csv(&mut self, data: &[u8]) -> Result<()> {
        println!("Initializing balances from airdrop 2 snapshot...");

        let recipients = Self::get_recipients_from_csv(data);

        let len = recipients[0].1.len();
        let mut totals = vec![0u64; len];

        for (_, networks) in recipients.iter() {
            for (i, (staked, count)) in networks.iter().enumerate() {
                let score = Self::score(*staked, *count);
                totals[i] += score;
            }
        }

        let precision = 1_000_000u128;
        let unom_per_network = AIRDROP_II_TOTAL / (len as u64);
        let unom_per_score: Vec<_> = totals
            .iter()
            .map(|n| unom_per_network as u128 * precision / *n as u128)
            .collect();

        let mut accounts = 0;
        let total_airdropped: u64 = recipients
            .iter()
            .map(|(addr, networks)| {
                let unom: u64 = networks
                    .iter()
                    .zip(unom_per_score.iter())
                    .map(|((staked, count), unom_per_score)| {
                        let score = Self::score(*staked, *count) as u128;
                        (score * unom_per_score / precision) as u64
                    })
                    .sum();

                self.airdrop_to(*addr, unom)?;
                accounts += 1;

                Ok(unom)
            })
            .sum::<Result<_>>()?;

        println!(
            "Total amount minted for airdrop 2: {} uNOM across {} accounts",
            total_airdropped, accounts,
        );

        Ok(())
    }

    fn airdrop_to(&mut self, addr: Address, unom: u64) -> Result<()> {
        let mut acct = self.accounts.entry(addr)?.or_insert_default()?;

        acct.btc_deposit.locked = unom / 3;
        acct.btc_withdraw.locked = unom / 3;
        acct.ibc_transfer.locked = unom / 3;

        Ok(())
    }

    fn score(staked: u64, _count: u64) -> u64 {
        staked.min(MAX_STAKED)
    }

    #[cfg(feature = "full")]
    fn get_recipients_from_csv(data: &[u8]) -> Vec<(Address, Vec<(u64, u64)>)> {
        let mut reader = csv::Reader::from_reader(data);

        reader
            .records()
            .filter_map(|row| {
                let row = row.unwrap();

                if row[0].len() != 44 {
                    return None;
                }
                let addr: Address = row[0].parse().unwrap();
                let values: Vec<_> = row
                    .into_iter()
                    .skip(1)
                    .map(|s| -> u64 { s.parse().unwrap() })
                    .collect();
                let pairs: Vec<_> = values.chunks_exact(2).map(|arr| (arr[0], arr[1])).collect();

                Some((addr, pairs))
            })
            .collect()
    }

    fn init_airdrop1_amount(
        &mut self,
        addr: Address,
        liquid: Amount,
        staked: Amount,
    ) -> Result<Amount> {
        let liquid_capped = Amount::min(liquid, 1_000_000_000.into());
        let staked_capped = Amount::min(staked, 1_000_000_000.into());

        let units = (liquid_capped + staked_capped * Amount::from(4))?;
        let units_per_nom = Decimal::from(20_299325) / Decimal::from(1_000_000);
        let nom_amount = (Decimal::from(units) / units_per_nom)?.amount()?;

        let mut acct = self.accounts.entry(addr)?.or_insert_default()?;
        acct.airdrop1.claimable = nom_amount.into();

        Ok(nom_amount)
    }

    #[cfg(feature = "full")]
    pub fn init_from_airdrop1_csv(&mut self, data: &[u8]) -> Result<()> {
        use std::convert::TryInto;

        let mut rdr = csv::Reader::from_reader(data);
        let snapshot = rdr.records();

        println!("Initializing balances from airdrop 1 snapshot...");

        let mut minted = Amount::from(0);
        let mut accounts = 0;

        for row in snapshot {
            let row = row.map_err(|e| Error::App(e.to_string()))?;

            let (_, address_b32, _) = bech32::decode(&row[0]).unwrap();
            let address_vec: Vec<u8> = bech32::FromBase32::from_base32(&address_b32).unwrap();
            let address_buf: [u8; 20] = address_vec.try_into().unwrap();

            let liquid: u64 = row[1].parse().unwrap();
            let staked: u64 = row[2].parse().unwrap();

            let minted_for_account =
                self.init_airdrop1_amount(address_buf.into(), liquid.into(), staked.into())?;
            minted = (minted + minted_for_account)?;
            accounts += 1;
        }

        println!(
            "Total amount minted for airdrop 1: {} uNOM across {} accounts",
            minted, accounts
        );

        Ok(())
    }
}

#[cfg(feature = "full")]
impl Migrate<nomicv3::app::Airdrop<nomicv3::app::Nom>> for Airdrop {
    fn migrate(&mut self, legacy: nomicv3::app::Airdrop<nomicv3::app::Nom>) -> Result<()> {
        println!("Migrating state of claimed airdrop 1 balances...");

        let mut claimed = vec![];

        for entry in self.accounts.iter()? {
            let (addr, acct) = entry?;

            if acct.airdrop1.claimable == 0 {
                continue;
            }

            let legacy_addr = addr.bytes().into();
            match legacy.balance(legacy_addr).unwrap() {
                None => claimed.push(*addr),
                Some(n) if n == 0 => claimed.push(*addr),
                Some(_) => continue,
            };
        }

        let claim_count = claimed.len();
        let mut total_claimed = 0;
        for addr in claimed {
            let mut acct = self.accounts.get_mut(addr)?.unwrap();
            let amount = acct.airdrop1.claimable;
            acct.airdrop1.claimable = 0;
            acct.airdrop1.claimed = amount;
            total_claimed += amount;
        }

        println!(
            "Airdrop 1 migration: {} uNOM has been claimed across {} accounts",
            total_claimed, claim_count,
        );

        Ok(())
    }
}

#[derive(State, Query, Call, Client, Clone, Debug, Default)]
pub struct Account {
    pub airdrop1: Part,
    pub btc_deposit: Part,
    pub btc_withdraw: Part,
    pub ibc_transfer: Part,
}

#[derive(State, Query, Call, Client, Clone, Debug, Default)]
pub struct Part {
    pub locked: u64,
    pub claimable: u64,
    pub claimed: u64,
}

impl Part {
    pub fn unlock(&mut self) {
        self.claimable += self.locked;
        self.locked = 0;
    }

    pub fn claim(&mut self) -> Result<u64> {
        let amount = self.claimable;
        if amount == 0 {
            return Err(Error::Coins("No balance to claim".to_string()));
        }

        self.claimed += amount;
        self.claimable = 0;
        Ok(amount)
    }
}