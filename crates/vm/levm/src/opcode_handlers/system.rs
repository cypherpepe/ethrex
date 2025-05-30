use crate::{
    call_frame::CallFrame,
    constants::{CREATE_DEPLOYMENT_FAIL, INIT_CODE_MAX_SIZE, REVERT_FOR_CALL, SUCCESS_FOR_CALL},
    db::cache,
    errors::{ExecutionReport, InternalError, OpcodeResult, OutOfGasError, TxResult, VMError},
    gas_cost::{self, max_message_call_gas, SELFDESTRUCT_REFUND},
    memory::{self, calculate_memory_size},
    precompiles::is_precompile,
    utils::{address_to_word, word_to_address, *},
    vm::{RetData, StateBackup, VM},
    Account,
};
use bytes::Bytes;
use ethrex_common::{types::Fork, Address, U256};

// System Operations (10)
// Opcodes: CREATE, CALL, CALLCODE, RETURN, DELEGATECALL, CREATE2, STATICCALL, REVERT, INVALID, SELFDESTRUCT

impl<'a> VM<'a> {
    // CALL operation
    pub fn op_call(&mut self) -> Result<OpcodeResult, VMError> {
        let (
            gas,
            callee,
            value_to_transfer,
            current_memory_size,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
        ) = {
            let current_call_frame = self.current_call_frame_mut()?;
            let gas = current_call_frame.stack.pop()?;
            let callee: Address = word_to_address(current_call_frame.stack.pop()?);
            let value_to_transfer: U256 = current_call_frame.stack.pop()?;
            let args_start_offset = current_call_frame.stack.pop()?;
            let args_size = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_| VMError::VeryLargeNumber)?;
            let return_data_start_offset = current_call_frame.stack.pop()?;
            let return_data_size: usize = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_| VMError::VeryLargeNumber)?;
            let current_memory_size = current_call_frame.memory.len();
            (
                gas,
                callee,
                value_to_transfer,
                current_memory_size,
                args_start_offset,
                args_size,
                return_data_start_offset,
                return_data_size,
            )
        };

        // VALIDATIONS
        if self.current_call_frame()?.is_static && !value_to_transfer.is_zero() {
            return Err(VMError::OpcodeNotAllowedInStaticContext);
        }

        // GAS
        let new_memory_size_for_args = calculate_memory_size(args_start_offset, args_size)?;
        let new_memory_size_for_return_data =
            calculate_memory_size(return_data_start_offset, return_data_size)?;
        let new_memory_size = new_memory_size_for_args.max(new_memory_size_for_return_data);

        let (account_info, address_was_cold) =
            access_account(self.db, &mut self.accrued_substate, callee)?;

        let (is_delegation, eip7702_gas_consumed, code_address, bytecode) =
            eip7702_get_code(self.db, &mut self.accrued_substate, callee)?;

        let gas_left = self
            .current_call_frame()?
            .gas_limit
            .checked_sub(self.current_call_frame()?.gas_used)
            .ok_or(InternalError::GasOverflow)?
            .checked_sub(eip7702_gas_consumed)
            .ok_or(InternalError::GasOverflow)?;

        let (cost, gas_limit) = gas_cost::call(
            new_memory_size,
            current_memory_size,
            address_was_cold,
            account_info.is_empty(),
            account_exists(self.db, callee),
            value_to_transfer,
            gas,
            gas_left,
            self.env.config.fork,
        )?;

        self.current_call_frame_mut()?.increase_consumed_gas(cost)?;
        self.current_call_frame_mut()?
            .increase_consumed_gas(eip7702_gas_consumed)?;

        let current_call_frame = self.current_call_frame()?;

        // OPERATION
        let msg_sender = current_call_frame.to; // The new sender will be the current contract.
        let to = callee; // In this case code_address and the sub-context account are the same. Unlike CALLCODE or DELEGATECODE.
        let is_static = current_call_frame.is_static;

        self.generic_call(
            gas_limit,
            value_to_transfer,
            msg_sender,
            to,
            code_address,
            true,
            is_static,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
            bytecode,
            is_delegation,
        )
    }

    // CALLCODE operation
    pub fn op_callcode(&mut self) -> Result<OpcodeResult, VMError> {
        // STACK
        let (
            gas,
            code_address,
            value_to_transfer,
            current_memory_size,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
        ) = {
            let current_call_frame = self.current_call_frame_mut()?;
            let gas = current_call_frame.stack.pop()?;
            let code_address = word_to_address(current_call_frame.stack.pop()?);
            let value_to_transfer = current_call_frame.stack.pop()?;
            let args_start_offset = current_call_frame.stack.pop()?;
            let args_size = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_err| VMError::VeryLargeNumber)?;
            let return_data_start_offset = current_call_frame.stack.pop()?;
            let return_data_size = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_err| VMError::VeryLargeNumber)?;
            let current_memory_size = current_call_frame.memory.len();
            (
                gas,
                code_address,
                value_to_transfer,
                current_memory_size,
                args_start_offset,
                args_size,
                return_data_start_offset,
                return_data_size,
            )
        };

        // GAS
        let new_memory_size_for_args = calculate_memory_size(args_start_offset, args_size)?;

        let new_memory_size_for_return_data =
            calculate_memory_size(return_data_start_offset, return_data_size)?;
        let new_memory_size = new_memory_size_for_args.max(new_memory_size_for_return_data);

        let (_account_info, address_was_cold) =
            access_account(self.db, &mut self.accrued_substate, code_address)?;

        let (is_delegation, eip7702_gas_consumed, code_address, bytecode) =
            eip7702_get_code(self.db, &mut self.accrued_substate, code_address)?;

        let gas_left = self
            .current_call_frame()?
            .gas_limit
            .checked_sub(self.current_call_frame()?.gas_used)
            .ok_or(InternalError::GasOverflow)?
            .checked_sub(eip7702_gas_consumed)
            .ok_or(InternalError::GasOverflow)?;

        let (cost, gas_limit) = gas_cost::callcode(
            new_memory_size,
            current_memory_size,
            address_was_cold,
            value_to_transfer,
            gas,
            gas_left,
            self.env.config.fork,
        )?;

        self.current_call_frame_mut()?.increase_consumed_gas(cost)?;
        self.current_call_frame_mut()?
            .increase_consumed_gas(eip7702_gas_consumed)?;

        let current_call_frame = self.current_call_frame()?;

        // Sender and recipient are the same in this case. But the code executed is from another account.
        let msg_sender = current_call_frame.to;
        let to = current_call_frame.to;
        let is_static = current_call_frame.is_static;

        self.generic_call(
            gas_limit,
            value_to_transfer,
            msg_sender,
            to,
            code_address,
            true,
            is_static,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
            bytecode,
            is_delegation,
        )
    }

    // RETURN operation
    pub fn op_return(&mut self) -> Result<OpcodeResult, VMError> {
        let current_call_frame = self.current_call_frame_mut()?;
        let offset = current_call_frame.stack.pop()?;
        let size = current_call_frame
            .stack
            .pop()?
            .try_into()
            .map_err(|_err| VMError::VeryLargeNumber)?;

        if size == 0 {
            return Ok(OpcodeResult::Halt);
        }

        let new_memory_size = calculate_memory_size(offset, size)?;
        let current_memory_size = current_call_frame.memory.len();

        current_call_frame
            .increase_consumed_gas(gas_cost::exit_opcode(new_memory_size, current_memory_size)?)?;

        current_call_frame.output =
            memory::load_range(&mut current_call_frame.memory, offset, size)?
                .to_vec()
                .into();

        Ok(OpcodeResult::Halt)
    }

    // DELEGATECALL operation
    pub fn op_delegatecall(&mut self) -> Result<OpcodeResult, VMError> {
        // https://eips.ethereum.org/EIPS/eip-7
        if self.env.config.fork < Fork::Homestead {
            return Err(VMError::InvalidOpcode);
        }
        // STACK
        let (
            gas,
            code_address,
            current_memory_size,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
        ) = {
            let current_call_frame = self.current_call_frame_mut()?;
            let gas = current_call_frame.stack.pop()?;
            let code_address = word_to_address(current_call_frame.stack.pop()?);
            let args_start_offset = current_call_frame.stack.pop()?;
            let args_size = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_err| VMError::VeryLargeNumber)?;
            let return_data_start_offset = current_call_frame.stack.pop()?;
            let return_data_size = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_err| VMError::VeryLargeNumber)?;
            let current_memory_size = current_call_frame.memory.len();
            (
                gas,
                code_address,
                current_memory_size,
                args_start_offset,
                args_size,
                return_data_start_offset,
                return_data_size,
            )
        };

        // GAS
        let (_account_info, address_was_cold) =
            access_account(self.db, &mut self.accrued_substate, code_address)?;

        let new_memory_size_for_args = calculate_memory_size(args_start_offset, args_size)?;
        let new_memory_size_for_return_data =
            calculate_memory_size(return_data_start_offset, return_data_size)?;
        let new_memory_size = new_memory_size_for_args.max(new_memory_size_for_return_data);

        let (is_delegation, eip7702_gas_consumed, code_address, bytecode) =
            eip7702_get_code(self.db, &mut self.accrued_substate, code_address)?;

        let gas_left = self
            .current_call_frame()?
            .gas_limit
            .checked_sub(self.current_call_frame()?.gas_used)
            .ok_or(InternalError::GasOverflow)?
            .checked_sub(eip7702_gas_consumed)
            .ok_or(InternalError::GasOverflow)?;

        let (cost, gas_limit) = gas_cost::delegatecall(
            new_memory_size,
            current_memory_size,
            address_was_cold,
            gas,
            gas_left,
            self.env.config.fork,
        )?;

        self.current_call_frame_mut()?.increase_consumed_gas(cost)?;
        self.current_call_frame_mut()?
            .increase_consumed_gas(eip7702_gas_consumed)?;

        let current_call_frame = self.current_call_frame()?;

        // OPERATION
        let msg_sender = current_call_frame.msg_sender;
        let value = current_call_frame.msg_value;
        let to = current_call_frame.to;
        let is_static = current_call_frame.is_static;

        self.generic_call(
            gas_limit,
            value,
            msg_sender,
            to,
            code_address,
            false,
            is_static,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
            bytecode,
            is_delegation,
        )
    }

    // STATICCALL operation
    pub fn op_staticcall(&mut self) -> Result<OpcodeResult, VMError> {
        // https://eips.ethereum.org/EIPS/eip-214
        if self.env.config.fork < Fork::Byzantium {
            return Err(VMError::InvalidOpcode);
        };
        // STACK
        let (
            gas,
            code_address,
            current_memory_size,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
        ) = {
            let current_call_frame = self.current_call_frame_mut()?;
            let gas = current_call_frame.stack.pop()?;
            let code_address = word_to_address(current_call_frame.stack.pop()?);
            let args_start_offset = current_call_frame.stack.pop()?;
            let args_size = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_err| VMError::VeryLargeNumber)?;
            let return_data_start_offset = current_call_frame.stack.pop()?;
            let return_data_size = current_call_frame
                .stack
                .pop()?
                .try_into()
                .map_err(|_err| VMError::VeryLargeNumber)?;
            let current_memory_size = current_call_frame.memory.len();
            (
                gas,
                code_address,
                current_memory_size,
                args_start_offset,
                args_size,
                return_data_start_offset,
                return_data_size,
            )
        };

        // GAS
        let (_account_info, address_was_cold) =
            access_account(self.db, &mut self.accrued_substate, code_address)?;

        let new_memory_size_for_args = calculate_memory_size(args_start_offset, args_size)?;
        let new_memory_size_for_return_data =
            calculate_memory_size(return_data_start_offset, return_data_size)?;
        let new_memory_size = new_memory_size_for_args.max(new_memory_size_for_return_data);

        let (is_delegation, eip7702_gas_consumed, _, bytecode) =
            eip7702_get_code(self.db, &mut self.accrued_substate, code_address)?;

        let gas_left = self
            .current_call_frame()?
            .gas_limit
            .checked_sub(self.current_call_frame()?.gas_used)
            .ok_or(InternalError::GasOverflow)?
            .checked_sub(eip7702_gas_consumed)
            .ok_or(InternalError::GasOverflow)?;

        let (cost, gas_limit) = gas_cost::staticcall(
            new_memory_size,
            current_memory_size,
            address_was_cold,
            gas,
            gas_left,
            self.env.config.fork,
        )?;

        self.current_call_frame_mut()?.increase_consumed_gas(cost)?;
        self.current_call_frame_mut()?
            .increase_consumed_gas(eip7702_gas_consumed)?;

        // OPERATION
        let value = U256::zero();
        let msg_sender = self.current_call_frame()?.to; // The new sender will be the current contract.
        let to = code_address; // In this case code_address and the sub-context account are the same. Unlike CALLCODE or DELEGATECODE.

        self.generic_call(
            gas_limit,
            value,
            msg_sender,
            to,
            code_address,
            true,
            true,
            args_start_offset,
            args_size,
            return_data_start_offset,
            return_data_size,
            bytecode,
            is_delegation,
        )
    }

    // CREATE operation
    pub fn op_create(&mut self) -> Result<OpcodeResult, VMError> {
        let fork = self.env.config.fork;
        let current_call_frame = self.current_call_frame_mut()?;
        let value_in_wei_to_send = current_call_frame.stack.pop()?;
        let code_offset_in_memory = current_call_frame.stack.pop()?;
        let code_size_in_memory: usize = current_call_frame
            .stack
            .pop()?
            .try_into()
            .map_err(|_err| VMError::VeryLargeNumber)?;

        let new_size = calculate_memory_size(code_offset_in_memory, code_size_in_memory)?;

        current_call_frame.increase_consumed_gas(gas_cost::create(
            new_size,
            current_call_frame.memory.len(),
            code_size_in_memory,
            fork,
        )?)?;

        self.generic_create(
            value_in_wei_to_send,
            code_offset_in_memory,
            code_size_in_memory,
            None,
        )
    }

    // CREATE2 operation
    pub fn op_create2(&mut self) -> Result<OpcodeResult, VMError> {
        // https://eips.ethereum.org/EIPS/eip-1014
        if self.env.config.fork < Fork::Constantinople {
            return Err(VMError::InvalidOpcode);
        }
        let fork = self.env.config.fork;
        let current_call_frame = self.current_call_frame_mut()?;
        let value_in_wei_to_send = current_call_frame.stack.pop()?;
        let code_offset_in_memory = current_call_frame.stack.pop()?;
        let code_size_in_memory: usize = current_call_frame
            .stack
            .pop()?
            .try_into()
            .map_err(|_err| VMError::VeryLargeNumber)?;
        let salt = current_call_frame.stack.pop()?;

        let new_size = calculate_memory_size(code_offset_in_memory, code_size_in_memory)?;

        current_call_frame.increase_consumed_gas(gas_cost::create_2(
            new_size,
            current_call_frame.memory.len(),
            code_size_in_memory,
            fork,
        )?)?;

        self.generic_create(
            value_in_wei_to_send,
            code_offset_in_memory,
            code_size_in_memory,
            Some(salt),
        )
    }

    // REVERT operation
    pub fn op_revert(&mut self) -> Result<OpcodeResult, VMError> {
        // Description: Gets values from stack, calculates gas cost and sets return data.
        // Returns: VMError RevertOpcode if executed correctly.
        // Notes:
        //      The actual reversion of changes is made in the execute() function.
        if self.env.config.fork < Fork::Byzantium {
            return Err(VMError::InvalidOpcode);
        }
        let current_call_frame = self.current_call_frame_mut()?;

        let offset = current_call_frame.stack.pop()?;

        let size = current_call_frame
            .stack
            .pop()?
            .try_into()
            .map_err(|_err| VMError::VeryLargeNumber)?;

        let new_memory_size = calculate_memory_size(offset, size)?;
        let current_memory_size = current_call_frame.memory.len();

        current_call_frame
            .increase_consumed_gas(gas_cost::exit_opcode(new_memory_size, current_memory_size)?)?;

        current_call_frame.output =
            memory::load_range(&mut current_call_frame.memory, offset, size)?
                .to_vec()
                .into();

        Err(VMError::RevertOpcode)
    }

    /// ### INVALID operation
    /// Reverts consuming all gas, no return data.
    pub fn op_invalid(&mut self) -> Result<OpcodeResult, VMError> {
        Err(VMError::InvalidOpcode)
    }

    // SELFDESTRUCT operation
    pub fn op_selfdestruct(&mut self) -> Result<OpcodeResult, VMError> {
        // Sends all ether in the account to the target address
        // Steps:
        // 1. Pop the target address from the stack
        // 2. Get current account and: Store the balance in a variable, set it's balance to 0
        // 3. Get the target account, checking if it is empty and if it is cold. Update gas cost accordingly.
        // 4. Add the balance of the current account to the target account
        // 5. Register account to be destroyed in accrued substate.
        // Notes:
        //      If context is Static, return error.
        //      If executed in the same transaction a contract was created, the current account is registered to be destroyed
        let (target_address, to) = {
            let current_call_frame = self.current_call_frame_mut()?;
            if current_call_frame.is_static {
                return Err(VMError::OpcodeNotAllowedInStaticContext);
            }
            let target_address = word_to_address(current_call_frame.stack.pop()?);
            let to = current_call_frame.to;
            (target_address, to)
        };
        let fork = self.env.config.fork;

        let (target_account_info, target_account_is_cold) =
            access_account(self.db, &mut self.accrued_substate, target_address)?;

        let (current_account_info, _current_account_is_cold) =
            access_account(self.db, &mut self.accrued_substate, to)?;
        let balance_to_transfer = current_account_info.balance;

        let account_is_empty = if self.env.config.fork >= Fork::SpuriousDragon {
            target_account_info.is_empty()
        } else {
            !account_exists(self.db, target_address)
        };
        self.current_call_frame_mut()?
            .increase_consumed_gas(gas_cost::selfdestruct(
                target_account_is_cold,
                account_is_empty,
                balance_to_transfer,
                fork,
            )?)?;

        // [EIP-6780] - SELFDESTRUCT only in same transaction from CANCUN
        if self.env.config.fork >= Fork::Cancun {
            increase_account_balance(self.db, target_address, balance_to_transfer)?;
            decrease_account_balance(self.db, to, balance_to_transfer)?;

            // Selfdestruct is executed in the same transaction as the contract was created
            if self.accrued_substate.created_accounts.contains(&to) {
                // If target is the same as the contract calling, Ether will be burnt.
                get_account_mut_vm(self.db, to)?.info.balance = U256::zero();

                self.accrued_substate.selfdestruct_set.insert(to);
            }
        } else {
            increase_account_balance(self.db, target_address, balance_to_transfer)?;
            get_account_mut_vm(self.db, to)?.info.balance = U256::zero();

            // [EIP-3529](https://eips.ethereum.org/EIPS/eip-3529)
            // https://github.com/ethereum/execution-specs/blob/master/src/ethereum/constantinople/vm/instructions/system.py#L471
            if self.env.config.fork < Fork::London
                && !self.accrued_substate.selfdestruct_set.contains(&to)
            {
                self.env.refunded_gas = self
                    .env
                    .refunded_gas
                    .checked_add(SELFDESTRUCT_REFUND)
                    .ok_or(VMError::GasRefundsOverflow)?;
            }

            self.accrued_substate.selfdestruct_set.insert(to);
        }
        if account_exists(self.db, target_address) && target_account_info.is_empty() {
            self.accrued_substate
                .touched_accounts
                .insert(target_address);
        }

        Ok(OpcodeResult::Halt)
    }

    /// Common behavior for CREATE and CREATE2 opcodes
    pub fn generic_create(
        &mut self,
        value_in_wei_to_send: U256,
        code_offset_in_memory: U256,
        code_size_in_memory: usize,
        salt: Option<U256>,
    ) -> Result<OpcodeResult, VMError> {
        let fork = self.env.config.fork;
        let (deployer_address, max_message_call_gas) = {
            let current_call_frame = self.current_call_frame_mut()?;
            // First: Validations that can cause out of gas.
            // 1. Cant be called in a static context
            if current_call_frame.is_static {
                return Err(VMError::OpcodeNotAllowedInStaticContext);
            }
            // 2. [EIP-3860] - Cant exceed init code max size
            if code_size_in_memory > INIT_CODE_MAX_SIZE && fork >= Fork::Shanghai {
                return Err(VMError::OutOfGas(OutOfGasError::ConsumedGasOverflow));
            }

            // Reserve gas for subcall
            let max_message_call_gas = max_message_call_gas(current_call_frame)?;
            current_call_frame.increase_consumed_gas(max_message_call_gas)?;

            // Clear callframe subreturn data
            current_call_frame.sub_return_data = Bytes::new();

            let deployer_address = current_call_frame.to;
            (deployer_address, max_message_call_gas)
        };

        let deployer_account_info =
            access_account(self.db, &mut self.accrued_substate, deployer_address)?.0;

        let code = Bytes::from(
            memory::load_range(
                &mut self.current_call_frame_mut()?.memory,
                code_offset_in_memory,
                code_size_in_memory,
            )?
            .to_vec(),
        );

        let new_address = match salt {
            Some(salt) => calculate_create2_address(deployer_address, &code, salt)?,
            None => calculate_create_address(deployer_address, deployer_account_info.nonce)?,
        };

        // touch account
        self.accrued_substate.touched_accounts.insert(new_address);

        let new_depth = {
            let current_call_frame = self.current_call_frame_mut()?;
            let new_depth = current_call_frame
                .depth
                .checked_add(1)
                .ok_or(InternalError::ArithmeticOperationOverflow)?;
            // SECOND: Validations that push 0 to the stack and return reserved_gas
            // 1. Sender doesn't have enough balance to send value.
            // 2. Depth limit has been reached
            // 3. Sender nonce is max.
            if deployer_account_info.balance < value_in_wei_to_send
                || new_depth > 1024
                || deployer_account_info.nonce == u64::MAX
            {
                // Return reserved gas
                current_call_frame.gas_used = current_call_frame
                    .gas_used
                    .checked_sub(max_message_call_gas)
                    .ok_or(VMError::Internal(InternalError::GasOverflow))?;
                // Push 0
                current_call_frame.stack.push(CREATE_DEPLOYMENT_FAIL)?;
                return Ok(OpcodeResult::Continue { pc_increment: 1 });
            }
            new_depth
        };

        // THIRD: Validations that push 0 to the stack without returning reserved gas but incrementing deployer's nonce
        let new_account = get_account(self.db, new_address)?;
        if new_account.has_code_or_nonce() {
            increment_account_nonce(self.db, deployer_address)?;
            self.current_call_frame_mut()?
                .stack
                .push(CREATE_DEPLOYMENT_FAIL)?;
            return Ok(OpcodeResult::Continue { pc_increment: 1 });
        }

        // FOURTH: Changes to the state
        // 1. Creating contract.

        // If the address has balance but there is no account associated with it, we need to add the value to it
        let new_balance = value_in_wei_to_send
            .checked_add(new_account.info.balance)
            .ok_or(VMError::BalanceOverflow)?;

        // https://github.com/ethereum/EIPs/blob/master/EIPS/eip-161.md
        let new_account = if self.env.config.fork < Fork::SpuriousDragon {
            Account::new(new_balance, Bytes::new(), 0, Default::default())
        } else {
            Account::new(new_balance, Bytes::new(), 1, Default::default())
        };
        cache::insert_account(&mut self.db.cache, new_address, new_account);

        // 2. Increment sender's nonce.
        increment_account_nonce(self.db, deployer_address)?;

        // 3. Decrease sender's balance.
        decrease_account_balance(self.db, deployer_address, value_in_wei_to_send)?;

        let new_call_frame = CallFrame::new(
            deployer_address,
            new_address,
            new_address,
            code,
            value_in_wei_to_send,
            Bytes::new(),
            false,
            max_message_call_gas,
            0,
            new_depth,
            true,
        );
        self.call_frames.push(new_call_frame);

        self.accrued_substate.created_accounts.insert(new_address); // Mostly for SELFDESTRUCT during initcode.

        self.return_data.push(RetData {
            is_create: true,
            ret_offset: U256::zero(),
            ret_size: 0,
            should_transfer_value: true,
            to: new_address,
            msg_sender: deployer_address,
            value: value_in_wei_to_send,
            max_message_call_gas,
        });
        // Backup of Database, Substate, Gas Refunds and Transient Storage if sub-context is reverted
        let backup = StateBackup::new(
            self.db.cache.clone(),
            self.accrued_substate.clone(),
            self.env.refunded_gas,
            self.env.transient_storage.clone(),
        );
        self.backups.push(backup);
        Ok(OpcodeResult::Continue { pc_increment: 0 })
    }

    #[allow(clippy::too_many_arguments)]
    /// This (should) be the only function where gas is used as a
    /// U256. This is because we have to use the values that are
    /// pushed to the stack.
    pub fn generic_call(
        &mut self,
        gas_limit: u64,
        value: U256,
        msg_sender: Address,
        to: Address,
        code_address: Address,
        should_transfer_value: bool,
        is_static: bool,
        args_offset: U256,
        args_size: usize,
        ret_offset: U256,
        ret_size: usize,
        bytecode: Bytes,
        is_delegation: bool,
    ) -> Result<OpcodeResult, VMError> {
        let sender_account_info =
            access_account(self.db, &mut self.accrued_substate, msg_sender)?.0;

        let calldata = {
            let current_call_frame = self.current_call_frame_mut()?;
            // Clear callframe subreturn data
            current_call_frame.sub_return_data = Bytes::new();

            let calldata =
                memory::load_range(&mut current_call_frame.memory, args_offset, args_size)?
                    .to_vec();

            // 1. Validate sender has enough value
            if should_transfer_value && sender_account_info.balance < value {
                current_call_frame.gas_used = current_call_frame
                    .gas_used
                    .checked_sub(gas_limit)
                    .ok_or(InternalError::GasOverflow)?;
                current_call_frame.stack.push(REVERT_FOR_CALL)?;
                return Ok(OpcodeResult::Continue { pc_increment: 1 });
            }
            calldata
        };

        let new_depth = {
            let current_call_frame = self.current_call_frame_mut()?;

            // 2. Validate max depth has not been reached yet.
            let new_depth = current_call_frame
                .depth
                .checked_add(1)
                .ok_or(InternalError::ArithmeticOperationOverflow)?;

            if new_depth > 1024 {
                current_call_frame.gas_used = current_call_frame
                    .gas_used
                    .checked_sub(gas_limit)
                    .ok_or(InternalError::GasOverflow)?;
                current_call_frame.stack.push(REVERT_FOR_CALL)?;
                return Ok(OpcodeResult::Continue { pc_increment: 1 });
            }

            if bytecode.is_empty() && is_delegation {
                current_call_frame.gas_used = current_call_frame
                    .gas_used
                    .checked_sub(gas_limit)
                    .ok_or(InternalError::GasOverflow)?;
                current_call_frame.stack.push(SUCCESS_FOR_CALL)?;
                return Ok(OpcodeResult::Continue { pc_increment: 1 });
            }
            new_depth
        };
        // Transfer value from caller to callee.
        if should_transfer_value {
            decrease_account_balance(self.db, msg_sender, value)?;
            increase_account_balance(self.db, to, value)?;
        }

        self.return_data.push(RetData {
            is_create: false,
            ret_offset,
            ret_size,
            should_transfer_value,
            to,
            msg_sender,
            value,
            max_message_call_gas: gas_limit,
        });
        let new_call_frame = CallFrame::new(
            msg_sender,
            to,
            code_address,
            bytecode,
            value,
            calldata.into(),
            is_static,
            gas_limit,
            0,
            new_depth,
            false,
        );
        self.call_frames.push(new_call_frame);
        // Backup of Database, Substate, Gas Refunds and Transient Storage if sub-context is reverted
        let backup = StateBackup::new(
            self.db.cache.clone(),
            self.accrued_substate.clone(),
            self.env.refunded_gas,
            self.env.transient_storage.clone(),
        );
        self.backups.push(backup);

        if is_precompile(&code_address, self.env.config.fork) {
            let _report = self.run_execution()?;
        }
        Ok(OpcodeResult::Continue { pc_increment: 0 })
    }

    pub fn handle_return(
        &mut self,
        call_frame: &CallFrame,
        tx_report: &ExecutionReport,
    ) -> Result<bool, VMError> {
        if call_frame.depth == 0 {
            self.call_frames.push(call_frame.clone());
            return Ok(false);
        }
        let retdata = self
            .return_data
            .pop()
            .ok_or(VMError::Internal(InternalError::CouldNotPopCallframe))?;
        if retdata.is_create {
            self.handle_return_create(tx_report, retdata)?;
        } else {
            self.handle_return_call(call_frame, tx_report, retdata)?;
        }
        Ok(true)
    }
    pub fn handle_return_call(
        &mut self,
        call_frame: &CallFrame,
        tx_report: &ExecutionReport,
        retdata: RetData,
    ) -> Result<(), VMError> {
        // Return gas left from subcontext
        let gas_left_from_new_call_frame = call_frame
            .gas_limit
            .checked_sub(tx_report.gas_used)
            .ok_or(InternalError::GasOverflow)?;
        {
            let current_call_frame = self.current_call_frame_mut()?;
            current_call_frame.gas_used = current_call_frame
                .gas_used
                .checked_sub(gas_left_from_new_call_frame)
                .ok_or(InternalError::GasOverflow)?;

            current_call_frame.logs.extend(tx_report.logs.clone());
            memory::try_store_range(
                &mut current_call_frame.memory,
                retdata.ret_offset,
                retdata.ret_size,
                &tx_report.output,
            )?;
            current_call_frame.sub_return_data = tx_report.output.clone();
        }

        // What to do, depending on TxResult
        match tx_report.result {
            TxResult::Success => {
                self.current_call_frame_mut()?
                    .stack
                    .push(SUCCESS_FOR_CALL)?;
            }
            TxResult::Revert(_) => {
                // Revert value transfer
                if retdata.should_transfer_value {
                    decrease_account_balance(self.db, retdata.to, retdata.value)?;
                    increase_account_balance(self.db, retdata.msg_sender, retdata.value)?;
                }
                // Push 0 to stack
                self.current_call_frame_mut()?.stack.push(REVERT_FOR_CALL)?;
            }
        }
        Ok(())
    }
    pub fn handle_return_create(
        &mut self,
        tx_report: &ExecutionReport,
        retdata: RetData,
    ) -> Result<(), VMError> {
        let unused_gas = retdata
            .max_message_call_gas
            .checked_sub(tx_report.gas_used)
            .ok_or(InternalError::GasOverflow)?;

        {
            let current_call_frame = self.current_call_frame_mut()?;
            // Return reserved gas
            current_call_frame.gas_used = current_call_frame
                .gas_used
                .checked_sub(unused_gas)
                .ok_or(InternalError::GasOverflow)?;

            current_call_frame.logs.extend(tx_report.logs.clone());
        }

        match tx_report.result.clone() {
            TxResult::Success => {
                self.current_call_frame_mut()?
                    .stack
                    .push(address_to_word(retdata.to))?;
            }
            TxResult::Revert(err) => {
                // Return value to sender
                increase_account_balance(self.db, retdata.msg_sender, retdata.value)?;

                // Deployment failed so account shouldn't exist
                cache::remove_account(&mut self.db.cache, &retdata.to);
                self.accrued_substate.created_accounts.remove(&retdata.to);

                let current_call_frame = self.current_call_frame_mut()?;
                // If revert we have to copy the return_data
                if err == VMError::RevertOpcode {
                    current_call_frame.sub_return_data = tx_report.output.clone();
                }
                current_call_frame.stack.push(CREATE_DEPLOYMENT_FAIL)?;
            }
        }
        Ok(())
    }
}
