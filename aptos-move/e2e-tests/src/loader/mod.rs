// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Logic for account universes. This is not in the parent module to enforce privacy.

use crate::{account::AccountData, executor::FakeExecutor};
use aptos_cached_packages::aptos_stdlib;
use aptos_framework::{BuildOptions, BuiltPackage};
use aptos_proptest_helpers::Index;
use aptos_temppath::TempPath;
use aptos_types::transaction::{
    EntryFunction, ExecutionStatus, SignedTransaction, TransactionStatus,
};
use move_core_types::{identifier::Identifier, language_storage::ModuleId, value::MoveValue};
use petgraph::{algo::toposort, graph::NodeIndex, Direction, Graph};
use proptest::{
    collection::{vec, SizeRange},
    prelude::*,
};
use std::cmp::Ordering;
mod module_generator;

const DEFAULT_BALANCE: u64 = 1_000_000_000;

#[derive(Debug)]
pub struct Node {
    name: ModuleId,
    self_value: u64,
    account_data: AccountData,
    expected_value: u64,
}

#[derive(Debug)]
pub struct DependencyGraph {
    graph: Graph<Node, ()>,
    base_directory: TempPath,
    sender_account: AccountData,
}

#[derive(Debug)]
pub enum LoaderTransactionGen {
    UpdateEdge(Index, Index),
    Invoke(Index),
}

// This module generates a sets of modules that could be used to test the loader.
//
// To generate the module, a DAG is first generated. The node in this graph represents a module and the edge in this graph represents a dependency
// between modules. For example if you generate the following DAG:
//
//          M1
//         /  \
//        M2  M3
//
// This will generate three modules: M1, M2 and M3. Where each module will look like following:
//
// module 0x3cf3846156a51da7251ccc84b5e4be10c5ab33049b7692d1164fd2c73ef3638b::M2 {
// public fun foo(): u64 {
//     a = 49252;                              // randomly generated integer for self node.
//     a
// }
// public entry fun foo_entry(expected_value: u64) {
//     assert!(Self::foo() == expected_value, 42);       // assert execution result matches expected value. Expected value can be derived from traversing the DAG.
// }
// }
//
// module e57d457d2ffacab8f46dea9779f8ad8135c68fd3574eb2902b3da0cfa67cf9d1::M3 {
// public fun foo(expected_value: u64): u64 {
//     a = 41973;                              // randomly generated integer for self node.
//     a
// }
// public entry fun foo_entry(expected_value: u64) {
//     assert!(Self::foo() == expected_value, 42);       // assert execution result matches expected value. Expected value can be derived from traversing the DAG.
// }
// }
//
// M2 and M3 are leaf nodes so the it should be a no-op function call. M1 will look like following:
//
// module 0x61da74259dae2c25beb41d11e43c6f5c00cc72d151d8a4f382b02e4e6c420d17::M1 {
// use 0x3cf3846156a51da7251ccc84b5e4be10c5ab33049b7692d1164fd2c73ef3638b::M2;
// use e57d457d2ffacab8f46dea9779f8ad8135c68fd3574eb2902b3da0cfa67cf9d1::M3;
// public fun foo(expected_value: u64): u64 {
//     a = 27856 + M2::foo(49252)+ M3::foo(41973);  // The dependency edge will be converted invocation into dependent module.
//     a                                            // Return the sum of self and all its dependent modules.
// }
// public entry fun foo_entry(expected_value: u64) {
//     assert!(Self::foo() == expected_value, 42);       // assert execution result matches expected value. Expected value can be derived from traversing the DAG.
// }
// }
//
// By using this strategy, we can generate a set of modules with complex depenency relationship and assert that the new loader is always
// linking the call to the right module. We can also invoke the entrypoint function to validate if the module dependencies have been
// resolved properly.
//
// TODOs:
// - randomly generate module upgrade request to mutate the structure of DAG to make sure the VM will be able to handle
// invaldation properly.
//
impl DependencyGraph {
    /// Returns a [`Strategy`] that generates a universe of accounts with pre-populated initial
    /// balances.
    ///
    /// Note that the real number of edges might be smaller than the size input provided due to our way of generating DAG.
    /// For example, if a (A, B) and (B, A) are both generated by the strategy, only one of them will be added to the graph.
    pub fn strategy(
        num_accounts: impl Into<SizeRange>,
        expected_num_edges: impl Into<SizeRange>,
    ) -> impl Strategy<Value = Self> {
        (
            vec(
                (
                    AccountData::strategy(DEFAULT_BALANCE..DEFAULT_BALANCE * 2),
                    any::<u16>(),
                    proptest::string::string_regex("[a-z]{10}").unwrap(),
                ),
                num_accounts,
            ),
            vec(any::<(Index, Index)>(), expected_num_edges),
        )
            .prop_map(move |(accounts, edge_indices)| Self::create(accounts, edge_indices))
    }

    fn create(accounts: Vec<(AccountData, u16, String)>, edges: Vec<(Index, Index)>) -> Self {
        let mut graph = Graph::new();
        let indices = accounts
            .into_iter()
            .map(|(account_data, self_value, module_name)| {
                graph.add_node(Node {
                    name: ModuleId::new(
                        *account_data.address(),
                        Identifier::new(module_name).unwrap(),
                    ),
                    self_value: self_value as u64,
                    account_data,
                    expected_value: 0,
                })
            })
            .collect::<Vec<_>>();

        for (lhs_idx, rhs_idx) in edges {
            let lhs = lhs_idx.get(&indices);
            let rhs = rhs_idx.get(&indices);

            match lhs.cmp(rhs) {
                Ordering::Greater => {
                    graph.add_edge(lhs.to_owned(), rhs.to_owned(), ());
                },
                Ordering::Less => {
                    graph.add_edge(rhs.to_owned(), lhs.to_owned(), ());
                },
                Ordering::Equal => (),
            }
        }
        let base_directory = TempPath::new();
        base_directory.create_as_dir().unwrap();
        Self {
            graph,
            base_directory,
            sender_account: AccountData::new(DEFAULT_BALANCE, 0),
        }
    }

    /// Set up [`DepGraph`] with the initial state generated in this universe.
    pub fn setup(&self, executor: &mut FakeExecutor) {
        for node in self.graph.raw_nodes().iter() {
            executor.add_account_data(&node.weight.account_data);
        }
        executor.add_account_data(&self.sender_account);
    }

    pub fn caculate_expected_values(&mut self) {
        let accounts = toposort(&self.graph, None).expect("Dep graph should be acyclic");
        for account_idx in accounts.iter().rev() {
            let mut result = 0;

            // Calculate the expected result of module entry function
            for successor in self
                .graph
                .neighbors_directed(*account_idx, Direction::Outgoing)
            {
                result += self
                    .graph
                    .node_weight(successor)
                    .expect("Topo sort should already compute the value for successor")
                    .expected_value;
            }
            let node = self
                .graph
                .node_weight_mut(*account_idx)
                .expect("Node should exist");

            node.expected_value = result + node.self_value;
        }
    }

    fn invoke_at(&mut self, node_idx: &NodeIndex) -> SignedTransaction {
        let txn = self
            .sender_account
            .account()
            .transaction()
            .sequence_number(self.sender_account.sequence_number())
            .entry_function(EntryFunction::new(
                self.graph.node_weight(*node_idx).unwrap().name.clone(),
                Identifier::new("foo_entry").unwrap(),
                vec![],
                vec![
                    MoveValue::U64(self.graph.node_weight(*node_idx).unwrap().expected_value)
                        .simple_serialize()
                        .unwrap(),
                ],
            ))
            .sign();

        self.sender_account.increment_sequence_number();
        txn
    }

    fn build_package_for_node(&mut self, node_idx: &NodeIndex) -> SignedTransaction {
        let node = self
            .graph
            .node_weight(*node_idx)
            .expect("Node should exist");
        let mut deps = vec![];

        for successor in self
            .graph
            .neighbors_directed(*node_idx, Direction::Outgoing)
        {
            deps.push(self.graph.node_weight(successor).unwrap().name.clone());
        }

        let package_path = module_generator::generate_package(
            &self.base_directory.path(),
            &node.name,
            &deps,
            node.self_value,
        );

        let package = BuiltPackage::build(package_path, BuildOptions::default()).unwrap();

        let code = package.extract_code();
        let metadata = package
            .extract_metadata()
            .expect("extracting package metadata must succeed");
        let txn = node
            .account_data
            .account()
            .transaction()
            .sequence_number(node.account_data.sequence_number())
            .payload(aptos_stdlib::code_publish_package_txn(
                bcs::to_bytes(&metadata).expect("PackageMetadata has BCS"),
                code,
            ))
            .sign();

        self.graph
            .node_weight_mut(*node_idx)
            .unwrap()
            .account_data
            .increment_sequence_number();
        txn
    }

    pub fn execute(
        &mut self,
        executor: &mut FakeExecutor,
        additional_txns: Vec<LoaderTransactionGen>,
    ) {
        // Generate a list of modules
        let accounts = toposort(&self.graph, None).expect("Dep graph should be acyclic");
        let mut txns = vec![];
        for account_idx in accounts.iter().rev() {
            let txn = self.build_package_for_node(account_idx);
            txns.push(txn);
        }

        for account_idx in accounts.iter() {
            txns.push(self.invoke_at(account_idx));
        }

        for txn_gen in additional_txns {
            if let Some(txn) = self.generate_txn(txn_gen) {
                txns.push(txn)
            }
        }

        let outputs = executor.execute_block(txns).unwrap();

        for output in outputs {
            assert_eq!(
                output.status(),
                &TransactionStatus::Keep(ExecutionStatus::Success)
            )
        }
    }

    pub fn generate_txn(&mut self, gen: LoaderTransactionGen) -> Option<SignedTransaction> {
        Some(match gen {
            LoaderTransactionGen::Invoke(idx) => {
                self.invoke_at(&NodeIndex::new(idx.index(self.graph.node_count())))
            },
            LoaderTransactionGen::UpdateEdge(lhs, rhs) => {
                let mut lhs_idx = NodeIndex::new(lhs.index(self.graph.node_count()));
                let mut rhs_idx = NodeIndex::new(rhs.index(self.graph.node_count()));
                match lhs_idx.cmp(&rhs_idx) {
                    Ordering::Greater => (),
                    Ordering::Less => std::mem::swap(&mut lhs_idx, &mut rhs_idx),
                    Ordering::Equal => return None,
                }
                if let Some(edge) = self.graph.find_edge(lhs_idx, rhs_idx) {
                    self.graph.remove_edge(edge);
                } else {
                    self.graph.add_edge(lhs_idx, rhs_idx, ());
                }

                self.caculate_expected_values();
                self.build_package_for_node(&lhs_idx)
            },
        })
    }
}

impl Arbitrary for LoaderTransactionGen {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with(_args: Self::Parameters) -> Self::Strategy {
        prop_oneof![
            9 => any::<Index>().prop_map(|idx| Self::Invoke(idx)),
            1 => any::<(Index, Index)>().prop_map(|(i1, i2)| Self::UpdateEdge(i1, i2)),
        ]
        .boxed()
    }
}
