use std::{collections::HashMap, rc::Rc, cell::RefCell};

use anyhow::{Result, bail};
use blockstack_lib::{
    burnchains::PoxConstants,
    chainstate::{
        burn::db::sortdb::SortitionDB,
        stacks::{db::StacksChainState, index::marf::MARFOpenOpts, StacksBlock},
    },
    core::{
        BITCOIN_MAINNET_FIRST_BLOCK_HASH, BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT,
        BITCOIN_MAINNET_FIRST_BLOCK_TIMESTAMP, STACKS_EPOCHS_MAINNET,
    },
};
use clarity::vm::{types::QualifiedContractIdentifier, database::{NULL_HEADER_DB, NULL_BURN_STATE_DB}, clarity::ClarityConnection, analysis::ContractAnalysis};
use diesel::{Connection, SqliteConnection, RunQueryDsl, sql_query};
use rand::Rng;
use stacks_common::types::chainstate::{BurnchainHeaderHash, StacksBlockId};
use log::*;

use crate::model::BlockHeader;

#[derive(Debug)]
pub struct TestContext {
    id: u64,
    baseline_env: Rc<RefCell<TestEnv>>,
    test_envs: HashMap<String, Rc<RefCell<TestEnv>>>,
}

impl TestContext {
    pub fn new(chainstate_path: &str) -> Result<Self> {
        let baseline_env = TestEnv::new(chainstate_path)?;

        Ok(Self {
            id: rand::thread_rng().gen_range(1000000000..9999999999),
            baseline_env: Rc::new(RefCell::new(baseline_env)),
            test_envs: Default::default(),
        })
    }

    pub fn with_baseline_env(&mut self, f: impl FnOnce(&TestContext, &TestEnvContext) -> Result<()>) -> Result<()> {
        let env_ctx = TestEnvContext::new(self, Rc::clone(&self.baseline_env));
        f(self, &env_ctx)?;
        Ok(())
    }

    pub fn new_env(&mut self, name: &str) -> Result<()> {
        let dir = format!("{}/{}/chainstate", std::env::temp_dir().display(), self.id);
        let env = Rc::new(RefCell::new(TestEnv::new(&dir)?));
        self.test_envs.insert(name.to_string(), env);
        Ok(())
    }

    pub fn with_env(&mut self, name: &str, f: impl FnOnce(&TestContext, Option<&mut TestEnvContext>) -> Result<()>) -> Result<()> {
        if let Some(env) = self.test_envs.get(name) {
            let env_ctx = TestEnvContext::new(self, Rc::clone(env));
            todo!()
        } else {
            f(self, None)?;
            Ok(())
        }
    }
}

pub struct TestEnv {
    chainstate_path: String,
    blocks_dir: String,
    chainstate: StacksChainState,
    index_db: SqliteConnection,
    sortition_db: SortitionDB,
}

impl std::fmt::Debug for TestEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestEnv")
            .field("chainstate_path", &self.chainstate_path)
            .field("blocks_dir", &self.blocks_dir)
            .field("chainstate", &"...")
            .field("index_db", &"...")
            .field("sortition_db", &"...")
            .finish()
    }
}

impl TestEnv {
    pub fn new(stacks_dir: &str) -> Result<Self> {
        let index_db_path = format!("{}/chainstate/vm/index.sqlite", stacks_dir);
        let sortition_db_path = format!("{}/burnchain/sortition", stacks_dir);
        let blocks_dir = format!("{}/chainstate/blocks", stacks_dir);
        let chainstate_path = format!("{}/chainstate", stacks_dir);

        debug!("index_db_path: '{}'", index_db_path);
        debug!("sortition_db_path: '{}'", sortition_db_path);
        debug!("blocks_dir: '{}'", blocks_dir);

        debug!("loading index db...");
        let index_db = SqliteConnection::establish(&index_db_path)?;
        info!("successfully connected to index db");

        let mut marf_opts = MARFOpenOpts::default();
        marf_opts.external_blobs = true;

        debug!("opening chainstate...");
        let chainstate = StacksChainState::open(
            true, 
            1, 
            &chainstate_path, 
            Some(marf_opts)
        )?;
        info!("successfully opened chainstate");

        debug!("opening sortition db...");
        let sortition_db = SortitionDB::connect(
            &sortition_db_path,
            BITCOIN_MAINNET_FIRST_BLOCK_HEIGHT,
            &BurnchainHeaderHash::from_hex(BITCOIN_MAINNET_FIRST_BLOCK_HASH).unwrap(),
            BITCOIN_MAINNET_FIRST_BLOCK_TIMESTAMP.into(),
            STACKS_EPOCHS_MAINNET.as_ref(),
            PoxConstants::mainnet_default(),
            false,
        )?;
        info!("successfully opened sortition db");

        Ok(Self {
            chainstate_path: chainstate_path.to_string(),
            blocks_dir,
            chainstate: chainstate.0,
            index_db,
            sortition_db,
        })
    }
}

#[derive(Debug)]
pub struct TestEnvContext<'a> {
    test_context: &'a TestContext,
    env: Rc<RefCell<TestEnv>>
}

impl<'a> TestEnvContext<'a> {
    pub fn new(test_context: &'a TestContext, env: Rc<RefCell<TestEnv>>) -> Self {
        Self {
            test_context,
            env
        }
    }

    pub fn load_contract_analysis(&self, at_block: &StacksBlockId, contract_id: &QualifiedContractIdentifier) -> Option<ContractAnalysis> {
        let mut env = self.env.borrow_mut();
        
        let mut conn = env.chainstate.clarity_state.read_only_connection(
            at_block, 
            &NULL_HEADER_DB, 
            &NULL_BURN_STATE_DB);

        conn.with_clarity_db_readonly_owned(|mut clarity_db| {
            (
                clarity_db.load_contract_analysis(contract_id),
                clarity_db
            )
        })
    }

    pub fn get_stacks_block(&self, block_hash: &str) -> Result<StacksBlock> {
        let env = self.env.borrow();

        let block_id = StacksBlockId::from_hex(block_hash)?;
        let block_path =
            StacksChainState::get_index_block_path(&env.blocks_dir, &block_id)?;
        let block = StacksChainState::consensus_load(&block_path)?;

        Ok(block)
    }
}

impl<'a> IntoIterator for &'a TestEnvContext<'a> {
    type Item = BlockHeader;
    type IntoIter = BlockIntoIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {

        let mut env = self.env.borrow_mut();
        let db = &mut env.index_db;

        let blocks_query = "
            SELECT DISTINCT
                parent.block_height, 
                parent.index_block_hash, 
                parent.parent_block_id 
            FROM block_headers parent 
            INNER JOIN block_headers child ON child.parent_block_id = parent.index_block_hash 
            ORDER BY parent.block_height ASC;";
        let mut blocks_result = sql_query(blocks_query)
            .get_results::<BlockHeader>(db)
            .expect("Failed to retrieve block inventory.");

        BlockIntoIterator {
            env_ctx: self,
            index: None,
            blocks: blocks_result.into_iter().map(Some).collect()
        }
    }
}

#[derive(Debug)]
pub struct BlockIntoIterator<'a> {
    env_ctx: &'a TestEnvContext<'a>,
    index: Option<usize>,
    blocks: Vec<Option<BlockHeader>>
}

impl<'a> Iterator for BlockIntoIterator<'a> {
    type Item = BlockHeader;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(index) = self.index {
            let next_index = index + 1;

            if next_index >= self.blocks.len() {
                return None;
            }

            self.index = Some(next_index);
            self.blocks[next_index].take()
            
        } else {
            self.index = Some(0);
            self.blocks[0].take()
        }
    }
}