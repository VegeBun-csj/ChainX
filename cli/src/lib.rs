// Copyright 2018 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Substrate CLI library.

#![warn(unused_extern_crates)]

extern crate tokio;

extern crate chainx_runtime;
extern crate exit_future;
extern crate substrate_cli as cli;
extern crate substrate_primitives as primitives;
#[cfg(test)]
extern crate substrate_service_test as service_test;
extern crate substrate_transaction_pool as transaction_pool;
#[macro_use]
extern crate substrate_network as network;
extern crate substrate_network_libp2p as network_p2p;
extern crate chainx_primitives;
extern crate substrate_client as client;
extern crate substrate_consensus_aura as consensus;
extern crate substrate_finality_grandpa as grandpa;
#[macro_use]
extern crate substrate_service;
extern crate chainx_executor;
extern crate substrate_inherents as inherents;
extern crate substrate_rpc_servers as rpc;

#[macro_use]
extern crate log;
extern crate structopt;

pub use cli::error;

mod chain_spec;
mod genesis_config;
mod native_rpc;
mod params;
mod service;

pub use cli::{IntoExit, VersionInfo};
use params::Params as NodeParams;
use primitives::ed25519;
use std::ops::Deref;
use structopt::StructOpt;
use substrate_service::{Roles as ServiceRoles, ServiceFactory};
use tokio::runtime::Runtime;

/// The chain specification option.
#[derive(Clone, Debug)]
pub enum ChainSpec {
    /// Whatever the current runtime is, with just Alice as an auth.
    Development,
    /// Whatever the current runtime is, with simple Alice/Bob auths.
    LocalTestnet,
    /// Whatever the current runtime is with the "global testnet" defaults.
    StagingTestnet,
}

/// Get a chain config from a spec setting.
impl ChainSpec {
    pub(crate) fn load(self) -> Result<chain_spec::ChainSpec, String> {
        Ok(match self {
            ChainSpec::Development => chain_spec::development_config(),
            ChainSpec::LocalTestnet => chain_spec::local_testnet_config(),
            ChainSpec::StagingTestnet => chain_spec::staging_testnet_config(),
        })
    }

    pub(crate) fn from(s: &str) -> Option<Self> {
        match s {
            // TODO wait for substrate fix for command sequence
            "dev" | "" => Some(ChainSpec::Development),
            "local" => Some(ChainSpec::LocalTestnet),
            "staging" => Some(ChainSpec::StagingTestnet),
            _ => None,
        }
    }
}

fn load_spec(id: &str) -> Result<Option<chain_spec::ChainSpec>, String> {
    Ok(match ChainSpec::from(id) {
        Some(spec) => Some(spec.load()?),
        None => None,
    })
}

pub fn run<I, T, E>(args: I, exit: E, version: cli::VersionInfo) -> error::Result<()>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString> + Clone,
    E: IntoExit,
{
    let full_version =
        substrate_service::config::full_version_from_strs(version.version, version.commit);

    let matches = match NodeParams::clap()
        .name(version.executable_name)
        .author(version.author)
        .about(version.description)
        .version(&(full_version + "\n")[..])
        .get_matches_from_safe(args)
    {
        Ok(m) => m,
        Err(e) => e.exit(),
    };

    let (spec, config) =
        cli::parse_matches::<service::Factory, _>(load_spec, &version, "chainx-node", &matches)?;

    //    if cfg!(feature = "msgbus-redis") == false {
    //        if matches.is_present("grandpa_authority_only") {
    //            // Authority Setup is only called if validator is set as true
    //            config.roles = ServiceRoles::AUTHORITY;
    //        } else if matches.is_present("grandpa_authority") {
    //            // Authority Setup is only called if validator is set as true
    //            config.roles = ServiceRoles::AUTHORITY;
    //        }
    //    }
    match cli::execute_default::<service::Factory, _>(spec, exit, &matches, &config)? {
        cli::Action::ExecutedInternally => (),
        cli::Action::RunService(exit) => {
            info!("ChainX");
            info!("  version {}", config.full_version());
            info!("  by ChainPool, ");
            info!("Chain specification: {}", config.chain_spec.name());
            info!("Node name: {}", config.name);
            info!("Roles: {:?}", config.roles);
            let mut runtime = Runtime::new()?;
            let executor = runtime.executor();
            match config.roles == ServiceRoles::LIGHT {
                true => run_until_exit(
                    &mut runtime,
                    service::Factory::new_light(config, executor)?,
                    exit,
                )?,
                false => run_until_exit(
                    &mut runtime,
                    service::Factory::new_full(config, executor)?,
                    exit,
                )?,
            }
        }
    }
    Ok(())
}

fn run_until_exit<T, C, E>(runtime: &mut Runtime, service: T, e: E) -> error::Result<()>
where
    T: Deref<Target = substrate_service::Service<C>> + native_rpc::Rpc,
    C: substrate_service::Components,
    E: IntoExit,
{
    let (exit_send, exit) = exit_future::signal();

    let executor = runtime.executor();
    let (_http, _ws) = service.start_rpc(executor.clone());
    cli::informant::start(&service, exit.clone(), executor.clone());

    let _ = runtime.block_on(e.into_exit());
    exit_send.fire();
    Ok(())
}
