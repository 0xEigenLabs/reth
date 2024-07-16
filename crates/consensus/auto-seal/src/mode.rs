//! The mode the auto seal miner is operating in.

use futures_util::{AsyncReadExt, stream::Fuse, StreamExt};
use reth_primitives::{TxHash};
use reth_transaction_pool::{PoolTransaction, TransactionPool, ValidPoolTransaction};
use std::{env, fmt, pin::Pin, sync::Arc, task::{Context, Poll}, time::Duration};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::{sync::mpsc::Receiver, time::Interval};
use tokio_stream::{wrappers::ReceiverStream, Stream};
use reth_primitives::hex::{FromHex, ToHex};
use tracing::*;
/// Mode of operations for the `Miner`
#[derive(Debug)]
pub enum MiningMode {
    /// A miner that does nothing
    None,
    /// A miner that listens for new transactions that are ready.
    ///
    /// Either one transaction will be mined per block, or any number of transactions will be
    /// allowed
    Auto(ReadyTransactionMiner),
    /// A miner that constructs a new block every `interval` tick
    FixedBlockTime(FixedBlockTimeMiner),
}

// === impl MiningMode ===

impl MiningMode {
    /// Creates a new instant mining mode that listens for new transactions and tries to build
    /// non-empty blocks as soon as transactions arrive.
    pub fn instant(max_transactions: usize, listener: Receiver<TxHash>) -> Self {
        MiningMode::Auto(ReadyTransactionMiner {
            max_transactions,
            has_pending_txs: None,
            rx: ReceiverStream::new(listener).fuse(),
        })
    }

    /// Creates a new interval miner that builds a block ever `duration`.
    pub fn interval(duration: Duration) -> Self {
        MiningMode::FixedBlockTime(FixedBlockTimeMiner::new(duration))
    }

    /// polls the Pool and returns those transactions that should be put in a block, if any.
    pub(crate) fn poll<Pool>(
        &mut self,
        pool: &Pool,
        cx: &mut Context<'_>,
    ) -> Poll<Vec<Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>>
    where
        Pool: TransactionPool,
    {
        match self {
            MiningMode::None => Poll::Pending,
            MiningMode::Auto(miner) => miner.poll(pool, cx),
            MiningMode::FixedBlockTime(miner) => miner.poll(pool, cx),
        }
    }
}

/// A miner that's supposed to create a new block every `interval`, mining all transactions that are
/// ready at that time.
///
/// The default blocktime is set to 6 seconds
#[derive(Debug)]
pub struct FixedBlockTimeMiner {
    /// The interval this fixed block time miner operates with
    interval: Interval,
}

// === impl FixedBlockTimeMiner ===

impl FixedBlockTimeMiner {
    /// Creates a new instance with an interval of `duration`
    pub(crate) fn new(duration: Duration) -> Self {
        let start = tokio::time::Instant::now() + duration;
        Self { interval: tokio::time::interval_at(start, duration) }
    }

    fn poll<Pool>(
        &mut self,
        pool: &Pool,
        cx: &mut Context<'_>,
    ) -> Poll<Vec<Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>>
    where
        Pool: TransactionPool,
    {
        if self.interval.poll_tick(cx).is_ready() {
            // drain the pool
            return Poll::Ready(pool.best_transactions().collect())
        }
        Poll::Pending
    }
}

impl Default for FixedBlockTimeMiner {
    fn default() -> Self {
        Self::new(Duration::from_secs(6))
    }
}

/// A miner that Listens for new ready transactions
pub struct ReadyTransactionMiner {
    /// how many transactions to mine per block
    max_transactions: usize,
    /// stores whether there are pending transactions (if known)
    has_pending_txs: Option<bool>,
    /// Receives hashes of transactions that are ready
    rx: Fuse<ReceiverStream<TxHash>>,
}

// === impl ReadyTransactionMiner ===

impl ReadyTransactionMiner {
    fn poll<Pool>(
        &mut self,
        pool: &Pool,
        cx: &mut Context<'_>,
    ) -> Poll<Vec<Arc<ValidPoolTransaction<<Pool as TransactionPool>::Transaction>>>>
    where
        Pool: TransactionPool,
    {
        // drain the notification stream
        while let Poll::Ready(Some(_hash)) = Pin::new(&mut self.rx).poll_next(cx) {
            self.has_pending_txs = Some(true);
        }

        if self.has_pending_txs == Some(false) {
            return Poll::Pending
        }

        // let transactions = pool.best_transactions().take(self.max_transactions).collect::<Vec<_>>();

        let flag = Arc::new(AtomicBool::new(false));

        let transactions = pool.best_transactions().filter(|tx| {
            
            info!(target: "consensus::auto-seal::miner::pool-tx-filter","tx info: {:?}", tx);
            // load contract addr and function selector
            let contract_address = env::var("BRIDGE_CONTRACT_ADDRESS").unwrap_or("0x".to_string());
            let bridge_asset_selector =  env::var("BRIDGE_ASSET_FUNCTION_SELECTOR").unwrap_or("0x".to_string());

            // check if the transaction is a bridge asset transaction
            let mut is_bridge_asset = false;

            let to = match tx.to(){
                Some(to) => to,
                None => return true
            };
            info!(target: "consensus::auto-seal::miner::pool-tx-filter","tx to: {:?}", to);

            let tx_input = tx.transaction.input();
            let tx_input_bytes: Vec<u8> = Vec::from_hex(tx_input.as_ref()).expect("err msg");
            let function_selector = &tx_input_bytes[0..4];
            let function_selector_str: String = function_selector.encode_hex();
            let _parameters_data = &tx_input_bytes[4..];
            info!(target: "consensus::auto-seal::miner::pool-tx-filter","tx function selector: {:?}", function_selector_str);

            // check if the transaction is a bridge asset transaction
            if to.to_string() == contract_address && function_selector_str == bridge_asset_selector {
                is_bridge_asset = true;
            }

            if !is_bridge_asset {
                return true
            }

            match flag.compare_exchange(false, true,  Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => {
                    true
                }
                Err(_) => {
                    false
                }
            }
        }).collect::<Vec<_>>();

        // there are pending transactions if we didn't drain the pool
        self.has_pending_txs = Some(transactions.len() >= self.max_transactions);

        if transactions.is_empty() {
            return Poll::Pending
        }

        Poll::Ready(transactions)
    }
}

impl fmt::Debug for ReadyTransactionMiner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadyTransactionMiner")
            .field("max_transactions", &self.max_transactions)
            .finish_non_exhaustive()
    }
}
