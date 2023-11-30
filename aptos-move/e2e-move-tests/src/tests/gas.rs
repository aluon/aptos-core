// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{tests::common::test_dir_path, MoveHarness};
use aptos_cached_packages::{aptos_stdlib, aptos_token_sdk_builder};
use aptos_crypto::{bls12381, PrivateKey, Uniform};
use aptos_gas_algebra::GasQuantity;
use aptos_gas_profiling::TransactionGasLog;
use aptos_language_e2e_tests::account::Account;
use aptos_transaction_generator_lib::{EntryPoints, publishing::{publish_util::PackageHandler, module_simple::MultiSigConfig}};
use aptos_types::{account_address::{default_stake_pool_address, AccountAddress}, fee_statement::{self, FeeStatement}, transaction::TransactionPayload};
use aptos_vm::AptosVM;
use rand::{rngs::StdRng, SeedableRng};
use std::path::Path;

fn save_profiling_results(name: &str, log: &TransactionGasLog) {
    let path = Path::new("gas-profiling").join(name);
    log.generate_html_report(path, format!("Gas Report - {}", name))
        .unwrap();
}

pub struct SummaryExeAndIO {
    pub intrinsic_cost: f64,
    pub execution_cost: f64,
    pub read_cost: f64,
    pub write_cost: f64,
}

fn summarize_exe_and_io(log: TransactionGasLog) -> SummaryExeAndIO {
    fn cast<T>(gas: GasQuantity<T>) -> f64 {
        u64::from(gas) as f64
    }

    let scale = cast(log.exec_io.gas_scaling_factor);

    let aggregated = log.exec_io.aggregate_gas_events();

    let execution = aggregated.ops.iter().map(|(_, _, v)| cast(*v)).sum::<f64>();
    let read = aggregated.storage_reads.iter().map(|(_, _, v)| cast(*v)).sum::<f64>();
    let write = aggregated.storage_writes.iter().map(|(_, _, v)| cast(*v)).sum::<f64>();
    SummaryExeAndIO {
        intrinsic_cost: cast(log.exec_io.intrinsic_cost) / scale,
        execution_cost: execution / scale,
        read_cost: read / scale,
        write_cost: write / scale,
    }
}

struct Runner {
    pub harness: MoveHarness,
    profile_gas: bool,
}

impl Runner {
    pub fn run(&mut self, function: &str, account: &Account, payload: TransactionPayload) {
        if !self.profile_gas {
            print_gas_cost(function, self.harness.evaluate_gas(account, payload));
        } else {
            let (log, gas_used, fee_statement) = self.harness.evaluate_gas_with_profiler(account, payload);
            save_profiling_results(function, &log);
            print_gas_cost_with_statement(function, gas_used, fee_statement);
        }
    }

    pub fn run_with_tps_estimate(&mut self, function: &str, account: &Account, payload: TransactionPayload, tps: f64) {
        if !self.profile_gas {
            print_gas_cost(function, self.harness.evaluate_gas(account, payload));
        } else {
            let (log, gas_used, fee_statement) = self.harness.evaluate_gas_with_profiler(account, payload);
            save_profiling_results(function, &log);
            print_gas_cost_with_statement_and_tps(function, gas_used, fee_statement, summarize_exe_and_io(log), tps);
        }
    }

    pub fn publish(&mut self, name: &str, account: &Account, path: &Path) {
        if !self.profile_gas {
            print_gas_cost(name, self.harness.evaluate_publish_gas(account, path));
        } else {
            let (log, gas_used, fee_statement) = self.harness.evaluate_publish_gas_with_profiler(account, path);
            save_profiling_results(name, &log);
            print_gas_cost_with_statement(name, gas_used, fee_statement);
        }
    }
}

/// Run with `cargo test test_gas -- --nocapture` to see output.
#[test]
fn test_gas() {
    // Start with 100 validators.
    let mut harness = MoveHarness::new_with_validators(100);
    let account_1 = &harness.new_account_at(AccountAddress::from_hex_literal("0x121").unwrap());
    let account_2 = &harness.new_account_at(AccountAddress::from_hex_literal("0x122").unwrap());
    let account_3 = &harness.new_account_at(AccountAddress::from_hex_literal("0x123").unwrap());
    let account_1_address = *account_1.address();
    let account_2_address = *account_2.address();
    let account_3_address = *account_3.address();

    // Use the gas profiler unless explicitly disabled by the user.
    //
    // This is to give us some basic code coverage on the gas profile.
    let profile_gas = match std::env::var("PROFILE_GAS") {
        Ok(s) => {
            let s = s.to_lowercase();
            s != "0" && s != "false" && s != "no"
        },
        Err(_) => true,
    };

    let mut runner = Runner { harness, profile_gas };

    AptosVM::set_paranoid_type_checks(true);

    print_gas_cost_with_statement_and_tps_header();

    let entry_points = vec![
        (5282., EntryPoints::Nop),
        (4583., EntryPoints::BytesMakeOrChange {
            data_length: Some(32),
        }),
        (3987., EntryPoints::StepDst),
        (2718., EntryPoints::InitializeVectorPicture { length: 40 }),
        (2718., EntryPoints::VectorPicture { length: 40 }),
        (2921., EntryPoints::VectorPictureRead { length: 40 }),
        (182., EntryPoints::InitializeVectorPicture { length: 30 * 1024 }),
        (182., EntryPoints::VectorPicture { length: 30 * 1024 }),
        (203., EntryPoints::VectorPictureRead { length: 30 * 1024 }),
        (19., EntryPoints::SmartTablePicture {
            length: 30 * 1024,
            num_points_per_txn: 200,
        }),
        (3.7, EntryPoints::SmartTablePicture {
            length: 1024 * 1024,
            num_points_per_txn: 1024,
        }),
        (1790., EntryPoints::TokenV1MintAndTransferFT),
        (1175., EntryPoints::TokenV1MintAndTransferNFTSequential),
        (1277., EntryPoints::TokenV2AmbassadorMint),
    ];

    for (tps, entry_point) in &entry_points {
        if let MultiSigConfig::None = entry_point.multi_sig_additional_num() {
            let publisher = runner.harness.new_account_with_key_pair();
            let user = runner.harness.new_account_with_key_pair();

            let mut package_handler = PackageHandler::new(entry_point.package_name());
            let mut rng = StdRng::seed_from_u64(14);
            let package = package_handler.pick_package(&mut rng, publisher.address().clone());
            runner.harness.run_transaction_payload(&publisher, package.publish_transaction_payload());
            if let Some(init_entry_point) = entry_point.initialize_entry_point() {
                runner.harness.run_transaction_payload(&publisher, init_entry_point.create_payload(package.get_module_id(init_entry_point.module_name()), Some(&mut rng), Some(publisher.address())));
            }

            runner.run_with_tps_estimate(
                &format!("entry_point_{entry_point:?}"),
                &user,
                entry_point.create_payload(package.get_module_id(entry_point.module_name()), Some(&mut rng), Some(publisher.address())),
                *tps,
            );
        } else {
            println!("Skipping multisig {entry_point:?}");
        }
    }

    runner.run_with_tps_estimate(
        "Transfer",
        account_1,
        aptos_stdlib::aptos_coin_transfer(account_2_address, 1000),
        3102.,
    );

    runner.run_with_tps_estimate(
        "CreateAccount",
        account_1,
        aptos_stdlib::aptos_account_create_account(
            AccountAddress::from_hex_literal("0xcafe1").unwrap(),
        ),
        2406.,
    );

    return;

    runner.run(
        "CreateTransfer",
        account_1,
        aptos_stdlib::aptos_account_transfer(
            AccountAddress::from_hex_literal("0xcafe2").unwrap(),
            1000,
        ),
    );
    runner.run(
        "CreateStakePool",
        account_1,
        aptos_stdlib::staking_contract_create_staking_contract(
            account_2_address,
            account_3_address,
            25_000_000,
            10,
            vec![],
        ),
    );
    let pool_address = default_stake_pool_address(account_1_address, account_2_address);
    let consensus_key = bls12381::PrivateKey::generate_for_testing();
    let consensus_pubkey = consensus_key.public_key().to_bytes().to_vec();
    let proof_of_possession = bls12381::ProofOfPossession::create(&consensus_key)
        .to_bytes()
        .to_vec();
    runner.run(
        "RotateConsensusKey",
        account_2,
        aptos_stdlib::stake_rotate_consensus_key(
            pool_address,
            consensus_pubkey,
            proof_of_possession,
        ),
    );
    runner.run(
        "JoinValidator100",
        account_2,
        aptos_stdlib::stake_join_validator_set(pool_address),
    );
    runner.run(
        "AddStake",
        account_1,
        aptos_stdlib::staking_contract_add_stake(account_2_address, 1000),
    );
    runner.run(
        "UnlockStake",
        account_1,
        aptos_stdlib::staking_contract_unlock_stake(account_2_address, 1000),
    );
    runner.harness.fast_forward(7200);
    runner.harness.new_epoch();
    runner.run(
        "WithdrawStake",
        account_1,
        aptos_stdlib::staking_contract_distribute(account_1_address, account_2_address),
    );
    runner.run(
        "LeaveValidatorSet100",
        account_2,
        aptos_stdlib::stake_leave_validator_set(pool_address),
    );
    let collection_name = "collection name".to_owned().into_bytes();
    let token_name = "token name".to_owned().into_bytes();
    runner.run(
        "CreateCollection",
        account_1,
        aptos_token_sdk_builder::token_create_collection_script(
            collection_name.clone(),
            "description".to_owned().into_bytes(),
            "uri".to_owned().into_bytes(),
            20_000_000,
            vec![false, false, false],
        ),
    );
    runner.run(
        "CreateTokenFirstTime",
        account_1,
        aptos_token_sdk_builder::token_create_token_script(
            collection_name.clone(),
            token_name.clone(),
            "collection description".to_owned().into_bytes(),
            1,
            4,
            "uri".to_owned().into_bytes(),
            account_1_address,
            1,
            0,
            vec![false, false, false, false, true],
            vec!["age".as_bytes().to_vec()],
            vec!["3".as_bytes().to_vec()],
            vec!["int".as_bytes().to_vec()],
        ),
    );
    runner.run(
        "MintToken",
        account_1,
        aptos_token_sdk_builder::token_mint_script(
            account_1_address,
            collection_name.clone(),
            token_name.clone(),
            1,
        ),
    );
    runner.run(
        "MutateToken",
        account_1,
        aptos_token_sdk_builder::token_mutate_token_properties(
            account_1_address,
            account_1_address,
            collection_name.clone(),
            token_name.clone(),
            0,
            1,
            vec!["age".as_bytes().to_vec()],
            vec!["4".as_bytes().to_vec()],
            vec!["int".as_bytes().to_vec()],
        ),
    );
    runner.run(
        "MutateToken2ndTime",
        account_1,
        aptos_token_sdk_builder::token_mutate_token_properties(
            account_1_address,
            account_1_address,
            collection_name.clone(),
            token_name.clone(),
            1,
            1,
            vec!["age".as_bytes().to_vec()],
            vec!["5".as_bytes().to_vec()],
            vec!["int".as_bytes().to_vec()],
        ),
    );

    let mut keys = vec![];
    let mut vals = vec![];
    let mut typs = vec![];
    for i in 0..10 {
        keys.push(format!("attr_{}", i).as_bytes().to_vec());
        vals.push(format!("{}", i).as_bytes().to_vec());
        typs.push("u64".as_bytes().to_vec());
    }
    runner.run(
        "MutateTokenAdd10NewProperties",
        account_1,
        aptos_token_sdk_builder::token_mutate_token_properties(
            account_1_address,
            account_1_address,
            collection_name.clone(),
            token_name.clone(),
            1,
            1,
            keys.clone(),
            vals.clone(),
            typs.clone(),
        ),
    );
    runner.run(
        "MutateTokenMutate10ExistingProperties",
        account_1,
        aptos_token_sdk_builder::token_mutate_token_properties(
            account_1_address,
            account_1_address,
            collection_name,
            token_name,
            1,
            1,
            keys,
            vals,
            typs,
        ),
    );

    let publisher = &harness.new_account_at(AccountAddress::from_hex_literal("0xcafe").unwrap());
    runner.publish(
        "PublishSmall",
        publisher,
        &test_dir_path("code_publishing.data/pack_initial"),
    );
    runner.publish(
        "UpgradeSmall",
        publisher,
        &test_dir_path("code_publishing.data/pack_upgrade_compat"),
    );
    let publisher = &harness.aptos_framework_account();
    runner.publish(
        "PublishLarge",
        publisher,
        &test_dir_path("code_publishing.data/pack_stdlib"),
    );
}

fn dollar_cost(gas_units: u64, price: u64) -> f64 {
    ((gas_units * 100/* gas unit price */) as f64) / 100_000_000_f64 * (price as f64)
}

pub fn print_gas_cost(function: &str, gas_units: u64) {
    println!(
        "{:8} | {:.6} | {:.6} | {:.6} | {}",
        gas_units,
        dollar_cost(gas_units, 5),
        dollar_cost(gas_units, 15),
        dollar_cost(gas_units, 30),
        function,
    );
}

pub fn print_gas_cost_with_statement(function: &str, gas_units: u64, fee_statement: Option<FeeStatement>) {
    println!(
        "{:8} | {:.6} | {:.6} | {:.6} | {:8} | {:8} | {:8} | {}",
        gas_units,
        dollar_cost(gas_units, 5),
        dollar_cost(gas_units, 15),
        dollar_cost(gas_units, 30),
        fee_statement.unwrap().execution_gas_used() + fee_statement.unwrap().io_gas_used(),
        fee_statement.unwrap().execution_gas_used(),
        fee_statement.unwrap().io_gas_used(),
        function,
    );
}

pub fn print_gas_cost_with_statement_and_tps_header() {
    println!(
        "{:9} | {:9.6} | {:9.6} | {:9.6} | {:8} | {:8} | {:8} | {:8} | {:8} | {:8} | {}",
        "gas units",
        "$ at 5",
        "$ at 15",
        "$ at 30",
        "exe+io g",
        // "exe gas",
        // "io gas",
        "intrins",
        "execut",
        "read",
        "write",
        "gas / s",
        "function",
    );
}

pub fn print_gas_cost_with_statement_and_tps(function: &str, gas_units: u64, fee_statement: Option<FeeStatement>, summary: SummaryExeAndIO, tps: f64) {
    println!(
        "{:9} | {:9.6} | {:9.6} | {:9.6} | {:8} | {:8.2} | {:8.2} | {:8.2} | {:8.2} | {:8.0} | {}",
        gas_units,
        dollar_cost(gas_units, 5),
        dollar_cost(gas_units, 15),
        dollar_cost(gas_units, 30),
        fee_statement.unwrap().execution_gas_used() + fee_statement.unwrap().io_gas_used(),
        // fee_statement.unwrap().execution_gas_used(),
        // fee_statement.unwrap().io_gas_used(),
        summary.intrinsic_cost,
        summary.execution_cost,
        summary.read_cost,
        summary.write_cost,
        (fee_statement.unwrap().execution_gas_used() + fee_statement.unwrap().io_gas_used()) as f64 * tps,
        function,
    );
}
