// Copyright (C) 2019-2021 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    helpers::{State, Status, Tasks},
    Environment,
    LedgerReader,
    LedgerRequest,
    LedgerRouter,
    Message,
    NodeType,
    PeersRequest,
    PeersRouter,
};
use snarkvm::dpc::prelude::*;

use anyhow::Result;
use rand::thread_rng;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    sync::{mpsc, oneshot, RwLock},
    task,
    task::JoinHandle,
};

/// Shorthand for the parent half of the `Prover` message channel.
pub(crate) type ProverRouter<N> = mpsc::Sender<ProverRequest<N>>;
#[allow(unused)]
/// Shorthand for the child half of the `Prover` message channel.
type ProverHandler<N> = mpsc::Receiver<ProverRequest<N>>;

///
/// An enum of requests that the `Prover` struct processes.
///
#[derive(Debug)]
pub enum ProverRequest<N: Network> {
    /// MemoryPoolClear := (block)
    MemoryPoolClear(Option<Block<N>>),
    /// UnconfirmedTransaction := (peer_ip, transaction)
    UnconfirmedTransaction(SocketAddr, Transaction<N>),
}

///
/// A prover for a specific network on the node server.
///
#[derive(Debug)]
pub struct Prover<N: Network, E: Environment> {
    /// The thread pool for the miner.
    miner: Arc<ThreadPool>,
    /// The prover router of the node.
    prover_router: ProverRouter<N>,
    /// The pool of unconfirmed transactions.
    memory_pool: RwLock<MemoryPool<N>>,
    /// The status of the node.
    status: Status,
    /// A terminator bit for the prover.
    terminator: Arc<AtomicBool>,
    /// The peers router of the node.
    peers_router: PeersRouter<N, E>,
    /// The ledger state of the node.
    ledger_reader: LedgerReader<N>,
    /// The ledger router of the node.
    ledger_router: LedgerRouter<N>,
}

impl<N: Network, E: Environment> Prover<N, E> {
    /// Initializes a new instance of the prover.
    pub async fn new(
        tasks: &mut Tasks<JoinHandle<()>>,
        miner: Option<Address<N>>,
        local_ip: SocketAddr,
        status: &Status,
        terminator: &Arc<AtomicBool>,
        peers_router: PeersRouter<N, E>,
        ledger_reader: &LedgerReader<N>,
        ledger_router: LedgerRouter<N>,
    ) -> Result<Arc<Self>> {
        // Initialize an mpsc channel for sending requests to the `Prover` struct.
        let (prover_router, mut prover_handler) = mpsc::channel(1024);
        // Initialize the prover pool.
        let pool = ThreadPoolBuilder::new()
            .stack_size(8 * 1024 * 1024)
            .num_threads((num_cpus::get() / 8 * 2).max(1))
            .build()?;

        // Initialize the prover.
        let prover = Arc::new(Self {
            miner: Arc::new(pool),
            prover_router,
            memory_pool: RwLock::new(MemoryPool::new()),
            status: status.clone(),
            terminator: terminator.clone(),
            peers_router,
            ledger_reader: ledger_reader.clone(),
            ledger_router,
        });

        // Initialize the handler for the prover.
        {
            let prover = prover.clone();
            let (router, handler) = oneshot::channel();
            tasks.append(task::spawn(async move {
                // Notify the outer function that the task is ready.
                let _ = router.send(());
                // Asynchronously wait for a prover request.
                while let Some(request) = prover_handler.recv().await {
                    // Hold the prover write lock briefly, to update the state of the prover.
                    prover.update(request).await;
                }
            }));
            // Wait until the prover handler is ready.
            let _ = handler.await;
        }

        // Initialize a new instance of the miner.
        if E::NODE_TYPE == NodeType::Miner {
            if let Some(recipient) = miner {
                // Initialize the prover process.
                let prover = prover.clone();
                let tasks_clone = tasks.clone();
                let (router, handler) = oneshot::channel();
                tasks.append(task::spawn(async move {
                    // Notify the outer function that the task is ready.
                    let _ = router.send(());
                    loop {
                        // If `terminator` is `false` and the status is `Ready`, mine the next block.
                        if !prover.terminator.load(Ordering::SeqCst) && prover.status.is_ready() {
                            // Set the status to `Mining`.
                            prover.status.update(State::Mining);

                            // Prepare the unconfirmed transactions, terminator, and status.
                            let miner = prover.miner.clone();
                            let canon = prover.ledger_reader.clone(); // This is *safe* as the ledger only reads.
                            let unconfirmed_transactions = prover.memory_pool.read().await.transactions();
                            let terminator = prover.terminator.clone();
                            let status = prover.status.clone();
                            let ledger_router = prover.ledger_router.clone();
                            let prover_router = prover.prover_router.clone();

                            tasks_clone.append(task::spawn(async move {
                                // Mine the next block.
                                let result = task::spawn_blocking(move || {
                                    miner.install(move || {
                                        canon.mine_next_block(recipient, &unconfirmed_transactions, &terminator, &mut thread_rng())
                                    })
                                })
                                .await
                                .map_err(|e| e.into());

                                // Set the status to `Ready`.
                                status.update(State::Ready);

                                match result {
                                    Ok(Ok(block)) => {
                                        debug!("Miner has found an unconfirmed candidate for block {}", block.height());
                                        // Broadcast the next block.
                                        let request = LedgerRequest::UnconfirmedBlock(local_ip, block, prover_router.clone());
                                        if let Err(error) = ledger_router.send(request).await {
                                            warn!("Failed to broadcast mined block: {}", error);
                                        }
                                    }
                                    Ok(Err(error)) | Err(error) => trace!("{}", error),
                                }
                            }));
                        }
                        // Sleep for 2 seconds.
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }));
                // Wait until the miner task is ready.
                let _ = handler.await;
            } else {
                error!("Missing miner address. Please specify an Aleo address in order to mine");
            }
        }

        Ok(prover)
    }

    /// Returns an instance of the prover router.
    pub fn router(&self) -> ProverRouter<N> {
        self.prover_router.clone()
    }

    ///
    /// Performs the given `request` to the prover.
    /// All requests must go through this `update`, so that a unified view is preserved.
    ///
    pub(super) async fn update(&self, request: ProverRequest<N>) {
        match request {
            ProverRequest::MemoryPoolClear(block) => match block {
                Some(block) => self.memory_pool.write().await.remove_transactions(block.transactions()),
                None => *self.memory_pool.write().await = MemoryPool::new(),
            },
            ProverRequest::UnconfirmedTransaction(peer_ip, transaction) => {
                // Ensure the node is not peering.
                if !self.status.is_peering() {
                    // Process the unconfirmed transaction.
                    self.add_unconfirmed_transaction(peer_ip, transaction).await
                }
            }
        }
    }

    ///
    /// Adds the given unconfirmed transaction to the memory pool.
    ///
    async fn add_unconfirmed_transaction(&self, peer_ip: SocketAddr, transaction: Transaction<N>) {
        // Process the unconfirmed transaction.
        trace!("Received unconfirmed transaction {} from {}", transaction.transaction_id(), peer_ip);
        // Ensure the unconfirmed transaction is new.
        if let Ok(false) = self.ledger_reader.contains_transaction(&transaction.transaction_id()) {
            debug!("Adding unconfirmed transaction {} to memory pool", transaction.transaction_id());
            // Attempt to add the unconfirmed transaction to the memory pool.
            match self.memory_pool.write().await.add_transaction(&transaction) {
                Ok(()) => {
                    // Upon success, propagate the unconfirmed transaction to the connected peers.
                    let request = PeersRequest::MessagePropagate(peer_ip, Message::UnconfirmedTransaction(transaction));
                    if let Err(error) = self.peers_router.send(request).await {
                        warn!("[UnconfirmedTransaction] {}", error);
                    }
                }
                Err(error) => error!("{}", error),
            }
        }
    }
}
