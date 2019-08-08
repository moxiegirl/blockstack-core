use std::collections::{HashMap, HashSet};
use std::fmt;

use vm::errors::{InterpreterError, UncheckedError, RuntimeErrorType, InterpreterResult as Result};
use vm::types::{Value, AssetIdentifier, PrincipalData};
use vm::callables::{DefinedFunction, FunctionIdentifier};
use vm::database::{ContractDatabase, ContractDatabaseTransacter};
use vm::{SymbolicExpression};
use vm::contracts::Contract;
use vm::{parser, eval};

use chainstate::burn::{VRFSeed, BlockHeaderHash};
use burnchains::BurnchainHeaderHash;

pub const MAX_CONTEXT_DEPTH: u16 = 256;

// TODO:
//    hide the environment's instance variables.
//     we don't want many of these changing after instantiation.
pub struct Environment <'a,'b> {
    pub global_context: &'a mut GlobalContext <'b>,
    pub contract_context: &'a ContractContext,
    pub call_stack: &'a mut CallStack,
    pub sender: Option<Value>,
    pub caller: Option<Value>
}

pub struct OwnedEnvironment <'a> {
    context: GlobalContext<'a>,
    default_contract: ContractContext,
    call_stack: CallStack
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssetMapEntry {
    Token(i128),
    Asset(Vec<Value>)
}

/**
 The AssetMap is used to track which assets have been transfered from whom
 during the execution of a transaction.
 */
#[derive(Debug)]
pub struct AssetMap {
    token_map: HashMap<PrincipalData, HashMap<AssetIdentifier, i128>>,
    asset_map: HashMap<PrincipalData, HashMap<AssetIdentifier, Vec<Value>>>
}

/** GlobalContext represents the outermost context for a transaction's
      execution. Logically, this context _never_ changes for the execution of
      transaction. However, due to the use of SavePoints for executing cross-contract
      calls, the GlobalContext can "nest", such that the inner-most GlobalContext may
      commit or abort its changes independent of the outer-most GlobalContext. Because
      of this, it may be easier to think of the GlobalContext as the "Database context".
      However, the GlobalContext also tracks some other variables which may only be
      modified during 
 */
pub struct GlobalContext <'a> {
    parent_map: Option<&'a mut AssetMap>,
    pub database: ContractDatabase<'a>,
    read_only: bool,
    asset_map: AssetMap
}

#[derive(Serialize, Deserialize)]
pub struct ContractContext {
    pub name: String,
    pub variables: HashMap<String, Value>,
    pub functions: HashMap<String, DefinedFunction>,
}

pub struct LocalContext <'a> {
    pub parent: Option< &'a LocalContext<'a>>,
    pub variables: HashMap<String, Value>,
    depth: u16
}

pub struct CallStack {
    stack: Vec<FunctionIdentifier>,
    set: HashSet<FunctionIdentifier>
}

pub type StackTrace = Vec<FunctionIdentifier>;

impl AssetMap {
    pub fn new() -> AssetMap {
        AssetMap {
            token_map: HashMap::new(),
            asset_map: HashMap::new()
        }
    }

    // This will get the next amount for a (principal, asset) entry in the asset table.
    fn get_next_amount(&self, principal: &PrincipalData, asset: &AssetIdentifier, amount: i128) -> Result<i128> {
        let current_amount = match self.token_map.get(principal) {
            Some(principal_map) => *principal_map.get(&asset).unwrap_or(&0),
            None => 0
        };
            
        current_amount.checked_add(amount)
            .ok_or(RuntimeErrorType::ArithmeticOverflow.into())
    }

    pub fn add_asset_transfer(&mut self, principal: &PrincipalData, asset: AssetIdentifier, transfered: Value) {
        if !self.asset_map.contains_key(principal) {
            self.asset_map.insert(principal.clone(), HashMap::new());
        }

        let principal_map = self.asset_map.get_mut(principal)
            .unwrap(); // should always exist, because of checked insert above.

        if principal_map.contains_key(&asset) {
            principal_map.get_mut(&asset).unwrap().push(transfered); 
        } else {
            principal_map.insert(asset, vec![transfered]); 
        }
    }

    pub fn add_token_transfer(&mut self, principal: &PrincipalData, asset: AssetIdentifier, amount: i128) -> Result<()> {
        if amount < 0 {
            panic!("Should never attempt to log a negative transfer.");
        }

        let next_amount = self.get_next_amount(principal, &asset, amount)?;

        if !self.token_map.contains_key(principal) {
            self.token_map.insert(principal.clone(), HashMap::new());
        }

        let principal_map = self.token_map.get_mut(principal)
            .unwrap(); // should always exist, because of checked insert above.

        principal_map.insert(asset, next_amount);

        Ok(())
    }

    // This will add any asset transfer data from other to self,
    //   aborting _all_ changes in the event of an error, leaving self unchanged
    pub fn commit_other(&mut self, mut other: AssetMap) -> Result<()> {
        let mut to_add = Vec::new();
        for (principal, mut principal_map) in other.token_map.drain() {
            for (asset, amount) in principal_map.drain() {
                let next_amount = self.get_next_amount(&principal, &asset, amount)?;
                to_add.push((principal.clone(), asset, next_amount));
            }
        }

        // After this point, this function will not fail.
        for (principal, mut principal_map) in other.asset_map.drain() {
            for (asset, mut transfers) in principal_map.drain() {
                if !self.asset_map.contains_key(&principal) {
                    self.asset_map.insert(principal.clone(), HashMap::new());
                }

                let landing_map = self.asset_map.get_mut(&principal)
                    .unwrap(); // should always exist, because of checked insert above.
                if landing_map.contains_key(&asset) {
                    let landing_vec = landing_map.get_mut(&asset).unwrap();
                    landing_vec.append(&mut transfers);
                } else {
                    landing_map.insert(asset, transfers);
                }
            }
        }


        for (principal, asset, amount) in to_add.drain(..) {
            if !self.token_map.contains_key(&principal) {
                self.token_map.insert(principal.clone(), HashMap::new());
            }

            let principal_map = self.token_map.get_mut(&principal)
                .unwrap(); // should always exist, because of checked insert above.
            principal_map.insert(asset, amount);
        }

        Ok(())
    }

    #[cfg(test)]
    pub fn to_table(mut self) -> HashMap<PrincipalData, HashMap<AssetIdentifier, AssetMapEntry>> {
        let mut map = HashMap::new();
        for (principal, mut principal_map) in self.token_map.drain() {
            let mut output_map = HashMap::new();
            for (asset, amount) in principal_map.drain() {
                output_map.insert(asset, AssetMapEntry::Token(amount));
            }
            map.insert(principal, output_map);
        }

        for (principal, mut principal_map) in self.asset_map.drain() {
            let output_map = if map.contains_key(&principal) {
                map.get_mut(&principal).unwrap()
            } else {
                map.insert(principal.clone(), HashMap::new());
                map.get_mut(&principal).unwrap()
            };

            for (asset, transfers) in principal_map.drain() {
                output_map.insert(asset, AssetMapEntry::Asset(transfers));
            }
        }

        return map
    }
}


#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_asset_map_abort() {
        let p1 = PrincipalData::ContractPrincipal("a".to_string());
        let p2 = PrincipalData::ContractPrincipal("b".to_string());

        let t1 = AssetIdentifier { contract_name: "a".to_string(), asset_name: "a".to_string() };
        let t2 = AssetIdentifier { contract_name: "b".to_string(), asset_name: "a".to_string() };

        let mut am1 = AssetMap::new();
        let mut am2 = AssetMap::new();

        am1.add_token_transfer(&p1, t1.clone(), 1).unwrap();
        am1.add_token_transfer(&p2, t1.clone(), i128::max_value()).unwrap();
        am2.add_token_transfer(&p1, t1.clone(), 1).unwrap();
        am2.add_token_transfer(&p2, t1.clone(), 1).unwrap();

        am1.commit_other(am2).unwrap_err();

        let table = am1.to_table();

        assert_eq!(table[&p2][&t1], AssetMapEntry::Token(i128::max_value()));
        assert_eq!(table[&p1][&t1], AssetMapEntry::Token(1));
    }

    #[test]
    fn test_asset_map_combinations() {
        let p1 = PrincipalData::ContractPrincipal("a".to_string());
        let p2 = PrincipalData::ContractPrincipal("b".to_string());
        let p3 = PrincipalData::ContractPrincipal("c".to_string());

        let t1 = AssetIdentifier { contract_name: "a".to_string(), asset_name: "a".to_string() };
        let t2 = AssetIdentifier { contract_name: "b".to_string(), asset_name: "a".to_string() };
        let t3 = AssetIdentifier { contract_name: "c".to_string(), asset_name: "a".to_string() };
        let t4 = AssetIdentifier { contract_name: "d".to_string(), asset_name: "a".to_string() };
        let t5 = AssetIdentifier { contract_name: "e".to_string(), asset_name: "a".to_string() };

        let mut am1 = AssetMap::new();
        let mut am2 = AssetMap::new();

        am1.add_token_transfer(&p1, t1.clone(), 10).unwrap();
        am2.add_token_transfer(&p1, t1.clone(), 15).unwrap();

        // test merging in a token that _didn't_ have an entry in the parent
        am2.add_token_transfer(&p1, t4.clone(), 1).unwrap();

        // test merging in a principal that _didn't_ have an entry in the parent
        am2.add_token_transfer(&p2, t2.clone(), 10).unwrap();
        am2.add_token_transfer(&p2, t2.clone(), 1).unwrap();

        // test merging in a principal that _didn't_ have an entry in the parent
        am2.add_asset_transfer(&p3, t3.clone(), Value::Int(10));

        // test merging in an asset that _didn't_ have an entry in the parent
        am1.add_asset_transfer(&p1, t5.clone(), Value::Int(0));
        am2.add_asset_transfer(&p1, t3.clone(), Value::Int(1));
        am2.add_asset_transfer(&p1, t3.clone(), Value::Int(0));

        // test merging in an asset that _does_ have an entry in the parent
        am1.add_asset_transfer(&p2, t3.clone(), Value::Int(2));
        am1.add_asset_transfer(&p2, t3.clone(), Value::Int(5));
        am2.add_asset_transfer(&p2, t3.clone(), Value::Int(3));
        am2.add_asset_transfer(&p2, t3.clone(), Value::Int(4));

        am1.commit_other(am2).unwrap();

        let table = am1.to_table();

        // 3 Principals
        assert_eq!(table.len(), 3);

        assert_eq!(table[&p1][&t1], AssetMapEntry::Token(25));
        assert_eq!(table[&p1][&t4], AssetMapEntry::Token(1));

        assert_eq!(table[&p2][&t2], AssetMapEntry::Token(11));

        assert_eq!(table[&p2][&t3], AssetMapEntry::Asset(
            vec![Value::Int(2), Value::Int(5), Value::Int(3), Value::Int(4)]));

        assert_eq!(table[&p1][&t3], AssetMapEntry::Asset(
            vec![Value::Int(1), Value::Int(0)]));
        assert_eq!(table[&p1][&t5], AssetMapEntry::Asset(
            vec![Value::Int(0)]));

        assert_eq!(table[&p3][&t3], AssetMapEntry::Asset(
            vec![Value::Int(10)]));
    }

}


impl fmt::Display for AssetMap {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "[")?;
        for (principal, principal_map) in self.token_map.iter() {
            for (asset, amount) in principal_map.iter() {
                write!(f, "{} spent {} {}\n", principal, amount, asset)?;
            }
        }
        for (principal, principal_map) in self.asset_map.iter() {
            for (asset, transfer) in principal_map.iter() {
                write!(f, "{} transfered [", principal)?;
                for t in transfer {
                    write!(f, "{}, ", t)?;
                }
                write!(f, "] {}\n", asset)?;
            }
        }
        write!(f, "]")
    }
}


impl <'a> OwnedEnvironment <'a> {
    pub fn new(database: &'a mut ContractDatabaseTransacter) -> OwnedEnvironment<'a> {
        OwnedEnvironment {
            context: GlobalContext::begin_from(database),
            default_contract: ContractContext::new(":transient:".to_string()),
            call_stack: CallStack::new()
        }
    }

    pub fn get_exec_environment <'b> (&'b mut self, sender: Option<Value>) -> Environment<'b,'a> {
        Environment::new(&mut self.context,
                         &self.default_contract,
                         &mut self.call_stack,
                         sender.clone(), sender)
    }

    pub fn initialize_contract(mut self, contract_name: &str, contract_content: &str) -> Result<()> {
        {
            let mut exec_env = self.get_exec_environment(None);
            exec_env.initialize_contract(contract_name, contract_content)?;
        }
        self.commit()?;
        Ok(())
    }

    pub fn execute_transaction(mut self, sender: Value, contract_name: &str, 
                               tx_name: &str, args: &[SymbolicExpression]) -> Result<(Value, AssetMap)> {
        let return_value = {
            let mut exec_env = self.get_exec_environment(Some(sender));
            exec_env.execute_contract(contract_name, tx_name, args)
        }?;
        let asset_map = self.commit()?;
        Ok((return_value, asset_map))
    }

    pub fn commit(self) -> Result<AssetMap> {
        self.context.commit()?
            .ok_or(InterpreterError::FailedToConstructAssetTable.into())
    }
}

impl <'a, 'b> Environment <'a, 'b> {
    // Environments pack a reference to the global context (which is basically the db),
    //   the current contract context, a call stack, and the current sender.
    // Essentially, the point of the Environment struct is to prevent all the eval functions
    //   from including all of these items in their method signatures individually. Because
    //   these different contexts can be mixed and matched (i.e., in a contract-call, you change
    //    contract context, or initiating a transaction necessitates a new globalcontext),
    //   a single "invocation" will end up creating multiple environment objects as context changes
    //    occur.
    pub fn new(global_context: &'a mut GlobalContext<'b>,
               contract_context: &'a ContractContext,
               call_stack: &'a mut CallStack,
               sender: Option<Value>, caller: Option<Value>) -> Environment<'a,'b> {
        if let Some(ref sender) = sender {
            if let Value::Principal(_) = sender {
            } else {
                panic!("Tried to construct environment with bad sender {}", sender);
            }
        }
        if let Some(ref caller) = caller {
            if let Value::Principal(_) = caller {
            } else {
                panic!("Tried to construct environment with bad caller {}", caller);
            }
        }

        Environment {
            global_context,
            contract_context,
            call_stack,
            sender,
            caller
        }
    }

    pub fn nest_as_principal <'c> (&'c mut self, sender: Value) -> Environment<'c, 'b> {
        Environment::new(self.global_context, self.contract_context, self.call_stack,
                         Some(sender.clone()), Some(sender))
    }

    pub fn nest_with_caller <'c> (&'c mut self, caller: Value) -> Environment<'c, 'b> {
        Environment::new(self.global_context, self.contract_context, self.call_stack,
                         self.sender.clone(), Some(caller))
    }

    pub fn eval_read_only(&mut self, contract_name: &str, program: &str) -> Result<Value> {
        let parsed = parser::parse(program)?;
        if parsed.len() < 1 {
            return Err(RuntimeErrorType::ParseError("Expected a program of at least length 1".to_string()).into())
        }

        let contract = self.global_context.database.get_contract(contract_name)?;
        let mut nested_context = self.global_context.nest();
        let result = {
            let mut nested_env = Environment::new(&mut nested_context, &contract.contract_context,
                                                  self.call_stack, self.sender.clone(), self.caller.clone());
            let local_context = LocalContext::new();
            eval(&parsed[0], &mut nested_env, &local_context)
        };
        nested_context.database.roll_back();

        result
    }
    
    pub fn eval_raw(&mut self, program: &str) -> Result<Value> {
        let parsed = parser::parse(program)?;
        if parsed.len() < 1 {
            return Err(RuntimeErrorType::ParseError("Expected a program of at least length 1".to_string()).into())
        }
        let local_context = LocalContext::new();
        let result = {
            eval(&parsed[0], self, &local_context)
        };
        result
    }

    pub fn execute_contract(&mut self, contract_name: &str, 
                            tx_name: &str, args: &[SymbolicExpression]) -> Result<Value> {
        let contract = self.global_context.database.get_contract(contract_name)?;

        let func = contract.contract_context.lookup_function(tx_name)
            .ok_or_else(|| { UncheckedError::UndefinedFunction(tx_name.to_string()) })?;
        if !func.is_public() {
            return Err(UncheckedError::NonPublicFunction(tx_name.to_string()).into());
        }

        let args: Result<Vec<Value>> = args.iter()
            .map(|arg| {
                let value = arg.match_atom_value()
                    .ok_or_else(|| InterpreterError::InterpreterError(format!("Passed non-value expression to exec_tx on {}!",
                                                                              tx_name)))?;
                Ok(value.clone())
            })
            .collect();

        let args = args?;

        self.execute_function_as_transaction(&func, &args, Some(&contract.contract_context)) 
    }

    pub fn execute_function_as_transaction(&mut self, function: &DefinedFunction, args: &[Value],
                                           next_contract_context: Option<&ContractContext>) -> Result<Value> {
        let make_read_only = function.is_read_only();

        let mut nested_context = {
            if make_read_only { 
                self.global_context.nest_read_only()
            } else {
                self.global_context.nest()
            }
        };

        let next_contract_context = next_contract_context.unwrap_or(self.contract_context);

        let result = {
            let mut nested_env = Environment::new(&mut nested_context, next_contract_context, self.call_stack,
                                                  self.sender.clone(), self.caller.clone());

            function.execute_apply(args, &mut nested_env)
        };

        if make_read_only {
            nested_context.database.roll_back();
            result
        } else {
            nested_context.handle_tx_result(result)
        }
    }

    pub fn initialize_contract(&mut self, contract_name: &str, contract_content: &str) -> Result<()> {
        let mut nested_context = self.global_context.nest();
        let result = Contract::initialize(contract_name, contract_content,
                                          &mut nested_context);
        match result {
            Ok(contract) => {
                nested_context.database.insert_contract(contract_name, contract);
                nested_context.commit()?;
                Ok(())
            },
            Err(e) => {
                nested_context.database.roll_back();
                Err(e)
            }
        }
    }
}

impl <'a> GlobalContext <'a> {
    
    pub fn new(database: ContractDatabase<'a>) -> GlobalContext<'a> {
        GlobalContext {
            parent_map: None,
            database: database,
            read_only: false,
            asset_map: AssetMap::new()
        }
    }

    pub fn log_asset_transfer(&mut self, sender: &PrincipalData, contract_name: &str, asset_name: &str, transfered: Value) {
        let asset_identifier = AssetIdentifier { contract_name: contract_name.to_string(),
                                                 asset_name: asset_name.to_string() };
        self.asset_map.add_asset_transfer(sender, asset_identifier, transfered)
    }

    pub fn log_token_transfer(&mut self, sender: &PrincipalData, contract_name: &str, asset_name: &str, transfered: i128) -> Result<()> {
        let asset_identifier = AssetIdentifier { contract_name: contract_name.to_string(),
                                                 asset_name: asset_name.to_string() };
        self.asset_map.add_token_transfer(sender, asset_identifier, transfered)
    }

    pub fn get_block_height(&self) -> u64 {
        self.database.get_simmed_block_height()
            .expect("Failed to obtain the current block height.")
    }

    pub fn get_block_time(&self, block_height: u64) -> u64 {
        self.database.get_simmed_block_time(block_height)
            .expect("Failed to obtain the block time for the given block height.")
    }

    pub fn get_block_header_hash(&self, block_height: u64) -> BlockHeaderHash {
        self.database.get_simmed_block_header_hash(block_height)
            .expect("Failed to obtain the block header hash for the given block height.")
    }

    pub fn get_burnchain_block_header_hash(&self, block_height: u64) -> BurnchainHeaderHash {
        self.database.get_simmed_burnchain_block_header_hash(block_height)
            .expect("Failed to obtain the burnchain block header hash for the given block height.")
    }

    pub fn get_block_vrf_seed(&self, block_height: u64) -> VRFSeed {
        self.database.get_simmed_block_vrf_seed(block_height)
            .expect("Failed to obtain the block vrf seed for the given block height.")
    }

    pub fn nest <'b> (&'b mut self) -> GlobalContext<'b> {
        let database = self.database.begin_save_point();

        GlobalContext {
            parent_map: Some(&mut self.asset_map),
            database: database,
            read_only: self.read_only,
            asset_map: AssetMap::new()
        }
    }

    pub fn nest_read_only <'b> (&'b mut self) -> GlobalContext<'b> {
        let database = self.database.begin_save_point();

        GlobalContext {
            parent_map: Some(&mut self.asset_map),
            database: database,
            read_only: true,
            asset_map: AssetMap::new()
        }
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    pub fn begin_from(database: &'a mut ContractDatabaseTransacter) -> GlobalContext<'a> {
        let db = database.begin_save_point();
        GlobalContext::new(db)
    }

    pub fn commit(self) -> Result<Option<AssetMap>> {
        let Self { parent_map, asset_map, database, .. } = self;

        let out_map = match parent_map {
            Some(parent_map) => { 
                parent_map.commit_other(asset_map)?;
                None
            },
            None => {
                Some(asset_map)
            }
        };

        database.commit();
        Ok(out_map)
    }

    pub fn handle_tx_result(mut self, result: Result<Value>) -> Result<Value> {
        if let Ok(result) = result {
            if let Value::Response(data) = result {
                if data.committed {
                    self.commit()?;
                } else {
                    self.database.roll_back();
                }
                Ok(Value::Response(data))
            } else {
                Err(UncheckedError::ContractMustReturnBoolean.into())
            }
        } else {
            self.database.roll_back();
            result
        }
    }
}

impl ContractContext {
    pub fn new(name: String) -> ContractContext {
        ContractContext {
            name: name,
            variables: HashMap::new(),
            functions: HashMap::new()
        }
    }

    pub fn lookup_variable(&self, name: &str) -> Option<Value> {
        match self.variables.get(name) {
            Some(value) => Option::Some(value.clone()),
            None => Option::None
        }
    }

    pub fn lookup_function(&self, name: &str) -> Option<DefinedFunction> {
        match self.functions.get(name) {
            Some(value) => Option::Some(value.clone()),
            None => Option::None
        }
    }
}

impl <'a> LocalContext <'a> {
    pub fn new() -> LocalContext<'a> {
        LocalContext {
            depth: 0,
            parent: Option::None,
            variables: HashMap::new(),
        }
    }
    
    pub fn extend(&'a self) -> Result<LocalContext<'a>> {
        if self.depth >= MAX_CONTEXT_DEPTH {
            Err(RuntimeErrorType::MaxContextDepthReached.into())
        } else {
            Ok(LocalContext {
                parent: Some(self),
                variables: HashMap::new(),
                depth: self.depth + 1
            })
        }
    }

    pub fn lookup_variable(&self, name: &str) -> Option<Value> {
        match self.variables.get(name) {
            Some(value) => Option::Some(value.clone()),
            None => {
                match self.parent {
                    Some(parent) => parent.lookup_variable(name),
                    None => Option::None
                }
            }
        }
    }
}

impl CallStack {
    pub fn new() -> CallStack {
        CallStack {
            stack: Vec::new(),
            set: HashSet::new()
        }
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn contains(&self, function: &FunctionIdentifier) -> bool {
        self.set.contains(function)
    }

    pub fn insert(&mut self, function: &FunctionIdentifier, track: bool) {
        self.stack.push(function.clone());
        if track {
            self.set.insert(function.clone());
        }
    }

    pub fn remove(&mut self, function: &FunctionIdentifier, tracked: bool) -> Result<()> {
        if let Some(removed) = self.stack.pop() {
            if removed != *function {
                return Err(InterpreterError::InterpreterError("Tried to remove item from empty call stack.".to_string()).into())
            }
            if tracked && !self.set.remove(&function) {
                panic!("Tried to remove tracked function from call stack, but could not find in current context.")
            }
            Ok(())
        } else {
            return Err(InterpreterError::InterpreterError("Tried to remove item from empty call stack.".to_string()).into())
        }
    }

    #[cfg(feature = "developer-mode")]
    pub fn make_stack_trace(&self) -> StackTrace {
        self.stack.clone()
    }

    #[cfg(not(feature = "developer-mode"))]
    pub fn make_stack_trace(&self) -> StackTrace {
        Vec::new()
    }
}
