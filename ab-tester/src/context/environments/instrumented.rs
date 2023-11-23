use std::cell::RefCell;
use std::rc::Rc;

use color_eyre::eyre::{anyhow, bail};
use color_eyre::Result;
use diesel::connection::SimpleConnection;
use diesel::query_builder::AsQuery;
use diesel::upsert::excluded;
use diesel::{
    debug_query, insert_into, upsert, Connection, ExpressionMethods, OptionalExtension, QueryDsl,
    RunQueryDsl, SqliteConnection,
};
use log::*;

use super::{BoxedDbIterResult, ReadableEnv, RuntimeEnv, WriteableEnv};
use crate::context::blocks::BlockHeader;
use crate::context::boot_data::mainnet_boot_data;
use crate::context::callbacks::{DefaultEnvCallbacks, RuntimeEnvCallbackHandler};
use crate::context::{
    Block, BlockCursor, BlockTransactionContext, Network, RegularBlockTransactionContext, Runtime,
    StacksEnvPaths,
};
use crate::db::appdb::AppDb;
use crate::db::dbcursor::stream_results;
use crate::db::model::app_db as model;
use crate::db::model::sortition_db::*;
use crate::db::schema::appdb::{self, _snapshots};
use crate::db::stacks_burnstate_db::StacksBurnStateDb;
use crate::db::stacks_headers_db::StacksHeadersDb;
use crate::{clarity, ok, stacks};

/// Holds the configuration of an [InstrumentedEnv].
pub struct InstrumentedEnvConfig {
    working_dir: String,
    readonly: bool,
    paths: StacksEnvPaths,
    runtime: Runtime,
    network: Network,
}

/// Holds the opened state of an [InstrumentedEnv].
pub struct InstrumentedEnvState {
    index_db_conn: RefCell<SqliteConnection>,
    chainstate: stacks::StacksChainState,
    clarity_db_conn: SqliteConnection,
    sortition_db_conn: Rc<RefCell<SqliteConnection>>,
    sortition_db: stacks::SortitionDB,
    burnstate_db: Box<dyn clarity::BurnStateDB>,
    headers_db: Box<dyn clarity::HeadersDB>,
}

/// This environment type is app-specific and will instrument all Clarity-related
/// operations. This environment can be used for comparisons.
pub struct InstrumentedEnv {
    id: i32,
    name: String,
    app_db: Rc<AppDb>,
    env_config: InstrumentedEnvConfig,
    env_state: Option<InstrumentedEnvState>,
    callbacks: Box<dyn RuntimeEnvCallbackHandler>,
}

impl InstrumentedEnv {
    /// Creates a new [InstrumentedEnv]. This method expects the provided
    /// `working_dir` to either be uninitialized or be using the same [Runtime]
    /// and [Network] configuration.
    pub fn new(
        id: i32,
        name: String,
        app_db: Rc<AppDb>,
        working_dir: String,
        runtime: Runtime,
        network: Network,
    ) -> Result<Self> {
        let paths = StacksEnvPaths::new(&working_dir);

        let env_config = InstrumentedEnvConfig {
            working_dir,
            readonly: false,
            paths,
            runtime,
            network,
        };

        Ok(Self {
            id,
            name,
            app_db,
            env_config,
            env_state: None,
            callbacks: Box::<DefaultEnvCallbacks>::default(),
        })
    }

    fn readonly(&mut self, readonly: bool) {
        self.env_config.readonly = readonly;
    }

    /// Attempts to retrieve the [InstrumentedEnvState] for this environment. Will
    /// return an error if [RuntimeEnv::open] has not been called.
    fn get_env_state(&self) -> Result<&InstrumentedEnvState> {
        let state = self
            .env_state
            .as_ref()
            .ok_or(anyhow!("[{}] environment has not been opened", self.name))?;

        Ok(state)
    }

    /// Attempts to retrieve the [InstrumentedEnvState] for this environment as a
    /// mutable reference. Will return an error if [RuntimeEnv::open] has not been called.
    fn get_env_state_mut(&mut self) -> Result<&mut InstrumentedEnvState> {
        let state = self
            .env_state
            .as_mut()
            .ok_or(anyhow!("[{}] environment has not been opened", self.name))?;

        Ok(state)
    }

    /// Retrieve all block headers from the underlying storage.
    fn block_headers(&self) -> Result<Vec<BlockHeader>> {
        // Get our state
        let state = self
            .env_state
            .as_ref()
            .ok_or(anyhow!("environment has not been opened"))?;

        // Retrieve the tip.
        self.callbacks.get_chain_tip_start(self);
        let tip = appdb::_block_headers::table
            .order_by(appdb::_block_headers::block_height.desc())
            .limit(1)
            .get_result::<model::BlockHeader>(&mut *state.index_db_conn.borrow_mut())?;
        // TODO: Handle when there is no tip (chain uninitialized).
        self.callbacks
            .get_chain_tip_finish(self, tip.block_height as u32);
        let mut current_block = Some(tip);

        // Vec for holding the headers we run into. This will initially be
        // in reverse order (from tip to genesis) - we reverse it later.
        let mut headers: Vec<BlockHeader> = Vec::new();

        // Walk backwards from tip to genesis, following the canonical fork. We
        // do this so that we don't follow orphaned blocks/forks.
        while let Some(block) = current_block {
            let block_parent = appdb::_block_headers::table
                .filter(appdb::_block_headers::index_block_hash.eq(&block.parent_block_id))
                .get_result::<model::BlockHeader>(&mut *state.index_db_conn.borrow_mut())
                .optional()?;

            if let Some(parent) = &block_parent {
                headers.push(BlockHeader::new(
                    block.block_height as u32,
                    hex::decode(block.index_block_hash)?,
                    hex::decode(block.parent_block_id)?,
                    hex::decode(block.consensus_hash)?,
                    hex::decode(&parent.consensus_hash)?,
                ));
            } else {
                headers.push(BlockHeader::new(
                    block.block_height as u32,
                    hex::decode(block.index_block_hash)?,
                    hex::decode(block.parent_block_id)?,
                    hex::decode(block.consensus_hash)?,
                    vec![0_u8; 20],
                ));
            }
            self.callbacks.load_block_headers_iter(self, headers.len());

            current_block = block_parent;
        }

        // Reverse the vec so that it is in block-ascending order.
        headers.reverse();

        debug!("first block: {:?}", headers[0]);
        debug!("tip: {:?}", headers[headers.len() - 1]);
        debug!("retrieved {} block headers", headers.len());

        self.callbacks
            .load_block_headers_finish(self, headers.len());
        Ok(headers)
    }
}

/// Implementation of [RuntimeEnv] for [InstrumentedEnv].
impl RuntimeEnv for InstrumentedEnv {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn is_readonly(&self) -> bool {
        self.env_config.readonly
    }

    fn is_open(&self) -> bool {
        self.env_state.is_some()
    }

    fn open(&mut self) -> Result<()> {
        let name = &self.name;
        let paths = &self.env_config.paths;
        let network = &self.env_config.network;

        info!("[{name}] opening environment...");
        self.callbacks.env_open_start(self, name);
        paths.print(name);

        // Setup our options for the Marf.
        let mut marf_opts = stacks::MARFOpenOpts::default();
        marf_opts.external_blobs = true;

        // Setup our boot data to be used if the chainstate hasn't been initialized yet.
        let mut boot_data = if network.is_mainnet() {
            mainnet_boot_data()
        } else {
            todo!("testnet not yet supported")
        };

        debug!("initializing chainstate");
        self.callbacks
            .open_chainstate_start(self, &paths.chainstate_dir);
        let (chainstate, _) = stacks::StacksChainState::open_and_exec(
            network.is_mainnet(),
            1,
            &paths.chainstate_dir,
            Some(&mut boot_data),
            Some(marf_opts.clone()),
        )?;
        self.callbacks.open_chainstate_finish(self);
        info!("[{name}] chainstate initialized.");

        debug!("[{name}] loading index db...");
        self.callbacks
            .open_index_db_start(self, &paths.index_db_path);
        let index_db_conn = SqliteConnection::establish(&paths.index_db_path)?;
        self.callbacks.open_index_db_finish(self);
        info!("[{name}] successfully connected to index db");

        // Open the Clarity sqlite db
        debug!("[{name}] loading clarity db...");
        self.callbacks
            .open_clarity_db_start(self, &paths.clarity_db_path);
        let clarity_db_conn = SqliteConnection::establish(&paths.clarity_db_path)?;
        self.callbacks.open_clarity_db_finish(self);
        info!("[{name}] successfully connected to clarity db");

        // Open the sortition db
        debug!("opening sortition dir: {}", &paths.sortition_dir);
        debug!("sortition db path: {}", &paths.sortition_db_path);
        self.callbacks
            .open_sortition_db_start(self, &paths.sortition_dir);
        let sortition_db = super::open_sortition_db(&paths.sortition_dir, network)?;
        let sortition_db_conn = SqliteConnection::establish(&paths.sortition_db_path)?;
        self.callbacks.open_sortition_db_finish(self);
        info!("successfully opened sortition db");

        // Open the burnstate db
        let burnstate_db: Box<dyn clarity::BurnStateDB> = Box::new(StacksBurnStateDb::new(
            &paths.sortition_db_path,
            boot_data.pox_constants,
        )?);

        // Open the headers db
        let headers_db: Box<dyn clarity::HeadersDB> =
            Box::new(StacksHeadersDb::new(&paths.index_db_path)?);

        let state = InstrumentedEnvState {
            chainstate,
            index_db_conn: RefCell::new(index_db_conn),
            clarity_db_conn,
            sortition_db_conn: Rc::new(RefCell::new(sortition_db_conn)),
            sortition_db,
            burnstate_db,
            headers_db,
        };

        self.env_state = Some(state);

        ok!()
    }
}

/// Implementation of [ReadableEnv] for [InstrumentedEnv].
impl ReadableEnv for InstrumentedEnv {
    fn blocks(&self) -> Result<BlockCursor> {
        let headers = self.block_headers()?;
        let cursor = BlockCursor::new(&self.env_config.paths.blocks_dir, headers);
        Ok(cursor)
    }

    fn snapshots(&self) -> BoxedDbIterResult<crate::types::Snapshot> {
        let state = self.get_env_state()?;

        let result = stream_results::<
            crate::db::model::app_db::Snapshot,
            crate::types::Snapshot,
            _,
            _,
        >(_snapshots::table, state.sortition_db_conn.clone(), 100);

        Ok(Box::new(result))
    }

    fn block_commits(&self) -> Result<Box<dyn Iterator<Item = Result<crate::types::BlockCommit>>>> {
        todo!()
    }

    fn ast_rules(&self) -> BoxedDbIterResult<crate::types::AstRuleHeight> {
        todo!()
    }

    fn epochs(&self) -> BoxedDbIterResult<crate::types::Epoch> {
        todo!()
    }
}

/// Implementation of [WriteableEnv] for [InstrumentedEnv].
impl WriteableEnv for InstrumentedEnv {
    fn block_begin(&mut self, block: &crate::context::Block) -> Result<BlockTransactionContext> {
        if self.is_readonly() {
            bail!("[{}] environment is read-only.", self.name);
        }

        let current_block_id: stacks::StacksBlockId;
        let next_block_id: stacks::StacksBlockId;

        warn!("block: {block:?}");

        match block {
            Block::Genesis(inner) => {
                current_block_id = inner.header.stacks_block_id()?;
                // TODO: Fix unwrap
                next_block_id = inner.next_header.as_ref().unwrap().stacks_block_id()?;

                let state = self.get_env_state_mut()?;

                info!("beginning genesis block");
                let clarity_tx = state.chainstate.block_begin(
                    &*state.burnstate_db,
                    &inner.header.parent_consensus_hash()?,
                    &inner.header.parent_block_hash()?,
                    &inner.header.consensus_hash()?,
                    &inner.header.stacks_block_hash()?,
                );

                info!("committing genesis block");
                clarity_tx.commit_to_block(
                    &inner.header.consensus_hash()?,
                    &inner.header.stacks_block_hash()?,
                );
            }
            Block::Regular(inner) => {
                current_block_id = inner.header.stacks_block_id()?;
                // TODO: Fix unwrap
                next_block_id = inner.next_header.as_ref().unwrap().stacks_block_id()?;
            }
        };

        info!("current_block_id: {current_block_id}, next_block_id: {next_block_id}");

        // Insert this block into the app database.
        self.app_db.insert_block(
            self.id,
            block.block_height()? as i32,
            block.block_hash()?.as_bytes(),
            block.index_block_hash()?,
        )?;

        let state = self.get_env_state_mut()?;

        // We cannot process genesis as it was already processed as a part of chainstate
        // initialization. Log that we reached it and skip processing.
        //if let Block::Genesis(_) = block {
        //info!("genesis block cannot be processed as it was statically initialized; moving on");
        //return Ok(BlockTransactionContext::Genesis);
        //}

        // Get an instance to the BurnStateDB (SortitionDB's `index_conn` implements this trait).

        // Start a new chainstate transaction on the index. This starts a new storage
        // transaction for the 'blocks' directory.

        debug!("beginning clarity block");
        let clarity_block_conn = state.chainstate.clarity_state.begin_block(
            &current_block_id,
            &next_block_id,
            &*state.headers_db,
            &*state.burnstate_db,
        );

        Ok(BlockTransactionContext::Regular(
            RegularBlockTransactionContext {
                stacks_block_id: current_block_id,
                clarity_block_conn,
                clarity_tx_conn: None,
            },
        ))
    }

    fn block_commit(
        &mut self,
        block_tx_ctx: BlockTransactionContext,
    ) -> Result<clarity::LimitedCostTracker> {
        if let BlockTransactionContext::Regular(ctx) = block_tx_ctx {
            //clarity_tx_conn.commit();
            //let cost_tracker = clarity_block_conn.commit_to_block(&stacks_block_id);
            //chainstate_tx.commit()?;
        } else {
            bail!("Cannot commit genesis block as it has already been statically loaded from boot data.")
        }

        todo!()
    }

    fn import_snapshots(
        &mut self,
        snapshots: Box<dyn Iterator<Item = Result<crate::types::Snapshot>>>,
    ) -> Result<()> {
        use crate::db::schema::sortition::snapshots;
        let state = self.get_env_state()?;

        info!("importing snapshots into '{}'...", self.name());
        let count: i64 = snapshots::table
            .count()
            .get_result(&mut *state.sortition_db_conn.borrow_mut())?;
        trace!("number of existing snapshots: {count}");

        state.sortition_db_conn.borrow_mut().transaction(|tx| -> Result<()> {
            for snapshot in snapshots {
                let snapshot: crate::db::model::sortition_db::Snapshot = snapshot?.try_into()?;

                trace!("inserting snapshot {{sortition_id: {:?}, index_root: {:?}}}", &snapshot.sortition_id, &snapshot.index_root);
                let insert_stmt = insert_into(snapshots::table)
                    .values(snapshot)
                    .on_conflict(snapshots::index_root)
                    .do_update()
                    .set(snapshots::index_root.eq(excluded(snapshots::index_root)));

                trace!("SQL: {}", debug_query::<diesel::sqlite::Sqlite, _>(&insert_stmt));

                let insert_result = insert_stmt.execute(tx)?;

                if insert_result != 1 {
                    bail!("expected insert of one snapshot, but got {insert_result} affected rows");
                }
            }

            ok!()
        })?;

        ok!()
    }

    fn import_block_commits(
        &mut self,
        block_commits: Box<dyn Iterator<Item = Result<crate::types::BlockCommit>>>,
    ) -> Result<()> {
        use crate::db::schema::sortition::block_commits;
        let state = self.get_env_state()?;

        info!("importing block commits into '{}'...", self.name());
        let count: i64 = block_commits::table
            .count()
            .get_result(&mut *state.sortition_db_conn.borrow_mut())?;
        trace!("number of existing block commits: {count}");

        state.sortition_db_conn.borrow_mut().transaction(|tx| -> Result<()> {
            for block_commit in block_commits {
                let block_commit: crate::db::model::sortition_db::BlockCommit = block_commit?.try_into()?;

                trace!("inserting block commit {{txid: {:?}, sortition_id: {:?}", &block_commit.txid, &block_commit.sortition_id);
                let insert_stmt = insert_into(block_commits::table)
                    .values(block_commit)
                    .on_conflict((block_commits::txid, block_commits::sortition_id))
                    .do_update()
                    .set((
                        block_commits::txid.eq(excluded(block_commits::txid)),
                        block_commits::sortition_id.eq(excluded(block_commits::sortition_id))
                    ));

                trace!("SQL: {}", debug_query::<diesel::sqlite::Sqlite, _>(&insert_stmt));

                let insert_result = insert_stmt.execute(tx)?;

                if insert_result != 1 {
                    bail!("expected insert of one block commit, but got {insert_result} affected rows");
                }
            }

            ok!()
        })?;

        ok!()
    }

    fn import_ast_rules(
        &mut self,
        ast_rules: Box<dyn Iterator<Item = Result<crate::types::AstRuleHeight>>>,
    ) -> Result<()> {
        todo!()
    }

    fn import_epochs(
        &mut self,
        ast_rules: Box<dyn Iterator<Item = Result<crate::types::Epoch>>>,
    ) -> Result<()> {
        todo!()
    }
}