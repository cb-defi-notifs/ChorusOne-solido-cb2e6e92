// SPDX-FileCopyrightText: 2021 Chorus One AG
// SPDX-License-Identifier: GPL-3.0

use solana_program::program::invoke_signed;
use solana_program::rent::Rent;
use solana_program::sysvar::Sysvar;
use solana_program::{account_info::AccountInfo, entrypoint::ProgramResult, msg, pubkey::Pubkey};

use crate::logic::check_rent_exempt;
use crate::processor::StakeType;
use crate::state::{Criteria, Lido, ValidatorPerf};
use crate::vote_state::PartialVoteState;
use crate::{
    error::LidoError,
    instruction::{
        AddMaintainerInfoV2, AddValidatorInfoV2, ChangeCriteriaInfo, ChangeRewardDistributionInfo,
        DeactivateIfViolatesInfo, DeactivateValidatorInfoV2, MergeStakeInfoV2,
        RemoveMaintainerInfoV2, RemoveValidatorInfoV2,
    },
    state::{ListEntry, Maintainer, RewardDistribution, Validator},
    vote_state::get_vote_account_commission,
    STAKE_AUTHORITY,
};

pub fn process_change_reward_distribution(
    program_id: &Pubkey,
    new_reward_distribution: RewardDistribution,
    accounts_raw: &[AccountInfo],
) -> ProgramResult {
    let accounts = ChangeRewardDistributionInfo::try_from_slice(accounts_raw)?;
    let mut lido = Lido::deserialize_lido(program_id, accounts.lido)?;
    lido.check_manager(accounts.manager)?;

    lido.check_is_st_sol_account(accounts.treasury_account)?;
    lido.check_is_st_sol_account(accounts.developer_account)?;

    lido.reward_distribution = new_reward_distribution;
    lido.fee_recipients.treasury_account = *accounts.treasury_account.key;
    lido.fee_recipients.developer_account = *accounts.developer_account.key;

    lido.save(accounts.lido)
}

pub fn process_add_validator(program_id: &Pubkey, accounts_raw: &[AccountInfo]) -> ProgramResult {
    let accounts = AddValidatorInfoV2::try_from_slice(accounts_raw)?;
    let lido = Lido::deserialize_lido(program_id, accounts.lido)?;
    let rent = &Rent::get()?;
    lido.check_manager(accounts.manager)?;

    check_rent_exempt(
        rent,
        accounts.validator_vote_account,
        "Validator vote account",
    )?;
    // Deserialize also checks if the vote account is a valid Solido vote
    // account: The vote account should be owned by the vote program and it should
    // satisfy the commission limit.
    let _partial_vote_state = PartialVoteState::deserialize(
        accounts.validator_vote_account,
        lido.criteria.max_commission,
    )?;

    let validator_list_data = &mut *accounts.validator_list.data.borrow_mut();
    let mut validators = lido.deserialize_account_list_info::<Validator>(
        program_id,
        accounts.validator_list,
        validator_list_data,
    )?;

    validators.push(Validator::new(*accounts.validator_vote_account.key))
}

/// Remove a validator.
///
/// This instruction is the final cleanup step in the validator removal process,
/// and it is callable by anybody. Initiation of the removal (`DeactivateValidator`)
/// is restricted to the manager, but once a validator is inactive, and there is
/// no more stake delegated to it, removing it from the list can be done by anybody.
pub fn process_remove_validator(
    program_id: &Pubkey,
    validator_index: u32,
    accounts_raw: &[AccountInfo],
) -> ProgramResult {
    let accounts = RemoveValidatorInfoV2::try_from_slice(accounts_raw)?;
    let lido = Lido::deserialize_lido(program_id, accounts.lido)?;

    let validator_list_data = &mut *accounts.validator_list.data.borrow_mut();
    let mut validators = lido.deserialize_account_list_info::<Validator>(
        program_id,
        accounts.validator_list,
        validator_list_data,
    )?;

    let removed_validator = validators.remove(
        validator_index,
        accounts.validator_vote_account_to_remove.key,
    )?;

    let result = removed_validator.check_can_be_removed();
    Validator::show_removed_error_msg(&result);
    result?;
    Ok(())
}

/// Set the `active` flag to false for a given validator.
///
/// This prevents new funds from being staked with this validator, and enables
/// removing the validator once no stake is delegated to it any more.
pub fn process_deactivate_validator(
    program_id: &Pubkey,
    validator_index: u32,
    accounts_raw: &[AccountInfo],
) -> ProgramResult {
    let accounts = DeactivateValidatorInfoV2::try_from_slice(accounts_raw)?;
    let lido = Lido::deserialize_lido(program_id, accounts.lido)?;
    lido.check_manager(accounts.manager)?;

    let validator_list_data = &mut *accounts.validator_list.data.borrow_mut();
    let mut validators = lido.deserialize_account_list_info::<Validator>(
        program_id,
        accounts.validator_list,
        validator_list_data,
    )?;

    let validator = validators.get_mut(
        validator_index,
        accounts.validator_vote_account_to_deactivate.key,
    )?;

    validator.deactivate();
    msg!("Validator {} deactivated.", validator.pubkey());
    Ok(())
}

/// Mark a validator inactive if any of their performance metrics exceeds the
/// allowed range of values.
///
/// This prevents new funds from being staked with this validator, and enables
/// removing the validator once no stake is delegated to it anymore.
pub fn process_deactivate_if_violates(
    program_id: &Pubkey,
    accounts_raw: &[AccountInfo],
) -> ProgramResult {
    let accounts = DeactivateIfViolatesInfo::try_from_slice(accounts_raw)?;
    let lido = Lido::deserialize_lido(program_id, accounts.lido)?;

    let validator_perf_list_data = &mut *accounts.validator_perf_list.data.borrow_mut();
    let validator_perfs = lido.deserialize_account_list_info::<ValidatorPerf>(
        program_id,
        accounts.validator_perf_list,
        validator_perf_list_data,
    )?;

    let validator_list_data = &mut *accounts.validator_list.data.borrow_mut();
    let mut validators = lido.deserialize_account_list_info::<Validator>(
        program_id,
        accounts.validator_list,
        validator_list_data,
    )?;

    // Find the validator in the list of validators.
    let validator = validators
        .iter_mut()
        .find(|validator| validator.pubkey() == accounts.validator_vote_account_to_deactivate.key);
    let validator = match validator {
        Some(validator) => validator,
        None => {
            msg!(
                "No such validator: {}.",
                accounts.validator_vote_account_to_deactivate.key
            );
            return Err(LidoError::InvalidAccountInfo.into());
        }
    };

    // Nothing to do if the validator is already inactive.
    if !validator.active {
        return Ok(());
    }

    let should_deactivate = if accounts.validator_vote_account_to_deactivate.owner
        == &solana_program::vote::program::id()
    {
        // Find the validator's performance metrics.
        let validator_perf = validator_perfs.iter().find(|perf| {
            &perf.validator_vote_account_address
                == accounts.validator_vote_account_to_deactivate.key
        });

        // And its commission.
        let data = accounts.validator_vote_account_to_deactivate.data.borrow();
        let commission = get_vote_account_commission(&data)?;

        // Check if the validator violates the criteria.
        let does_perform_well =
            validator_perf.map_or(true, |perf| perf.meets_criteria(&lido.criteria));
        let does_perform_well = does_perform_well && commission <= lido.criteria.max_commission;

        // If the validator does not perform well, deactivate it.
        !does_perform_well
    } else {
        // The vote account is closed by node operator.
        true
    };
    if !should_deactivate {
        return Ok(());
    }

    validator.deactivate();
    msg!("Validator {} deactivated.", validator.pubkey());

    Ok(())
}

/// Adds a maintainer to the list of maintainers
pub fn process_add_maintainer(program_id: &Pubkey, accounts_raw: &[AccountInfo]) -> ProgramResult {
    let accounts = AddMaintainerInfoV2::try_from_slice(accounts_raw)?;
    let lido = Lido::deserialize_lido(program_id, accounts.lido)?;
    lido.check_manager(accounts.manager)?;

    let maintainer_list_data = &mut *accounts.maintainer_list.data.borrow_mut();
    let mut maintainers = lido.deserialize_account_list_info::<Maintainer>(
        program_id,
        accounts.maintainer_list,
        maintainer_list_data,
    )?;

    maintainers.push(Maintainer::new(*accounts.maintainer.key))
}

/// Removes a maintainer from the list of maintainers
pub fn process_remove_maintainer(
    program_id: &Pubkey,
    maintainer_index: u32,
    accounts_raw: &[AccountInfo],
) -> ProgramResult {
    let accounts = RemoveMaintainerInfoV2::try_from_slice(accounts_raw)?;
    let lido = Lido::deserialize_lido(program_id, accounts.lido)?;
    lido.check_manager(accounts.manager)?;

    let maintainer_list_data = &mut *accounts.maintainer_list.data.borrow_mut();
    let mut maintainers = lido.deserialize_account_list_info::<Maintainer>(
        program_id,
        accounts.maintainer_list,
        maintainer_list_data,
    )?;

    maintainers.remove(maintainer_index, accounts.maintainer.key)?;
    Ok(())
}

/// Set the new curation criteria. If validators exceed those thresholds,
/// they will be deactivated by `DeactivateIfViolates`.
pub fn process_change_criteria(
    program_id: &Pubkey,
    new_criteria: Criteria,
    accounts_raw: &[AccountInfo],
) -> ProgramResult {
    if new_criteria.max_commission > 100 {
        return Err(LidoError::ValidationCommissionOutOfBounds.into());
    }

    let accounts = ChangeCriteriaInfo::try_from_slice(accounts_raw)?;
    let mut lido = Lido::deserialize_lido(program_id, accounts.lido)?;

    lido.check_manager(accounts.manager)?;

    lido.criteria = new_criteria;

    lido.save(accounts.lido)
}

/// Merge two stake accounts from the beginning of the validator's stake
/// accounts list.
/// This function can be called by anybody.
/// After this function, the validator's `stake_accounts_seed_begin` ceases to
/// exist and is merged with the stake defined by `stake_accounts_seed_begin +
/// 1`, and `stake_accounts_seed_begin` is incremented by one.
/// All fully active stake accounts precede the activating stake accounts.
pub fn process_merge_stake(
    program_id: &Pubkey,
    validator_index: u32,
    accounts_raw: &[AccountInfo],
) -> ProgramResult {
    let accounts = MergeStakeInfoV2::try_from_slice(accounts_raw)?;
    let lido = Lido::deserialize_lido(program_id, accounts.lido)?;

    let validator_list_data = &mut *accounts.validator_list.data.borrow_mut();
    let mut validator = lido.deserialize_account_list_info::<Validator>(
        program_id,
        accounts.validator_list,
        validator_list_data,
    )?;

    let validator = validator.get_mut(validator_index, accounts.validator_vote_account.key)?;

    let from_seed = validator.stake_seeds.begin;
    let to_seed = validator.stake_seeds.begin + 1;

    // Check that there are at least two accounts to merge
    if to_seed >= validator.stake_seeds.end {
        msg!("Attempting to merge accounts in a validator that has fewer than two stake accounts.");
        return Err(LidoError::InvalidStakeAccount.into());
    }

    // Recalculate the `from_stake`.
    let (from_stake_addr, _) = validator.find_stake_account_address(
        program_id,
        accounts.lido.key,
        from_seed,
        StakeType::Stake,
    );
    // Compare with the stake passed in `accounts`.
    if &from_stake_addr != accounts.from_stake.key {
        msg!(
            "Calculated from_stake {} for seed {} is different from received {}.",
            from_stake_addr,
            from_seed,
            accounts.from_stake.key
        );
        return Err(LidoError::InvalidStakeAccount.into());
    }
    let (to_stake_addr, _) = validator.find_stake_account_address(
        program_id,
        accounts.lido.key,
        to_seed,
        StakeType::Stake,
    );
    if &to_stake_addr != accounts.to_stake.key {
        msg!(
            "Calculated to_stake {} for seed {} is different from received {}.",
            to_stake_addr,
            to_seed,
            accounts.to_stake.key
        );
        return Err(LidoError::InvalidStakeAccount.into());
    }
    validator.stake_seeds.begin += 1;
    // Merge `from_stake_addr` to `to_stake_addr`, at the end of the
    // instruction, `from_stake_addr` ceases to exist.
    let merge_instructions = solana_program::stake::instruction::merge(
        &to_stake_addr,
        &from_stake_addr,
        accounts.stake_authority.key,
    );

    // For some reason, `merge` returns a `Vec`, but when we look at the
    // implementation, we can see that it always returns a single instruction.
    assert_eq!(merge_instructions.len(), 1);
    let merge_instruction = &merge_instructions[0];

    invoke_signed(
        merge_instruction,
        &[
            accounts.from_stake.clone(),
            accounts.to_stake.clone(),
            accounts.sysvar_clock.clone(),
            accounts.stake_history.clone(),
            accounts.stake_authority.clone(),
            accounts.stake_program.clone(),
        ],
        &[&[
            &accounts.lido.key.to_bytes(),
            STAKE_AUTHORITY,
            &[lido.stake_authority_bump_seed],
        ]],
    )?;

    Ok(())
}
