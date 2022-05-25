pub mod blockstore;
pub mod ext;
pub mod state;
pub mod types;
mod utils;

use fil_actor_hierarchical_sca::{FundParams, Method, MIN_COLLATERAL_AMOUNT};
use fvm_ipld_encoding::{RawBytes, DAG_CBOR};
use fvm_sdk as sdk;
use fvm_sdk::NO_DATA_BLOCK_ID;
use fvm_shared::address::Address;
use fvm_shared::bigint::bigint_ser::BigIntDe;
use fvm_shared::econ::TokenAmount;
use fvm_shared::{ActorID, METHOD_SEND};
use num_traits::Zero;

use crate::blockstore::*;
use crate::state::{get_stake, State};
use crate::types::*;
use crate::utils::*;

/// The actor's WASM entrypoint. It takes the ID of the parameters block,
/// and returns the ID of the return value block, or NO_DATA_BLOCK_ID if no
/// return value.
///
/// Should probably have macros similar to the ones on fvm.filecoin.io snippets.
/// Put all methods inside an impl struct and annotate it with a derive macro
/// that handles state serde and dispatch.
#[no_mangle]
pub fn invoke(params: u32) -> u32 {
    let params = sdk::message::params_raw(params).unwrap().1;
    let params = RawBytes::new(params);
    // Conduct method dispatch. Handle input parameters and return data.
    let ret: anyhow::Result<Option<RawBytes>> = match sdk::message::method_number() {
        1 => Actor::constructor(deserialize_params(&params).unwrap()),
        2 => Actor::join(),
        3 => Actor::leave(),
        4 => Actor::kill(),
        // 5 => Actor::submit_checkpoint(),
        _ => abort!(USR_UNHANDLED_MESSAGE, "unrecognized method"),
    };

    // Insert the return data block if necessary, and return the correct
    // block ID.
    match ret {
        Ok(None) => NO_DATA_BLOCK_ID,
        Ok(Some(v)) => match sdk::ipld::put_block(DAG_CBOR, v.bytes()) {
            Ok(id) => id,
            Err(e) => abort!(USR_SERIALIZATION, "failed to store return value: {}", e),
        },
        Err(e) => abort!(USR_ILLEGAL_STATE, "error calling method: {}", e),
    }
}

pub trait SubnetActor {
    fn constructor(params: ConstructParams) -> anyhow::Result<Option<RawBytes>>;
    fn join() -> anyhow::Result<Option<RawBytes>>;
    fn leave() -> anyhow::Result<Option<RawBytes>>;
    fn kill() -> anyhow::Result<Option<RawBytes>>;
    // fn submit_checkpoint() -> Option<RawBytes>;
}

pub struct Actor;

impl SubnetActor for Actor {
    /// The constructor populates the initial state.
    ///
    /// Method num 1. This is part of the Filecoin calling convention.
    /// InitActor#Exec will call the constructor on method_num = 1.
    fn constructor(params: ConstructParams) -> anyhow::Result<Option<RawBytes>> {
        // This constant should be part of the SDK.
        const INIT_ACTOR_ADDR: ActorID = 1;

        // Should add SDK sugar to perform ACL checks more succinctly.
        // i.e. the equivalent of the validate_* builtin-actors runtime methods.
        // https://github.com/filecoin-project/builtin-actors/blob/master/actors/runtime/src/runtime/fvm.rs#L110-L146
        const TEST: ActorID = 339;
        let is_test = State::is_test();
        if sdk::message::caller() != INIT_ACTOR_ADDR && (sdk::message::caller() != TEST && is_test)
        {
            abort!(USR_FORBIDDEN, "constructor invoked by non-init actor");
        }

        let state = State::new(params, is_test);
        state.save();
        Ok(None)
    }

    fn join() -> anyhow::Result<Option<RawBytes>> {
        let mut st = State::load();
        let caller = Address::new_id(sdk::message::caller());
        let amount = sdk::message::value_received();
        if amount <= TokenAmount::zero() {
            abort!(
                USR_ILLEGAL_ARGUMENT,
                "a minimum collateral is required to join the subnet"
            );
        }
        // increase collateral
        st.add_stake(&caller, &amount)?;
        // if we have enough collateral, register in SCA
        if st.status == Status::Instantiated {
            if sdk::sself::current_balance() >= TokenAmount::from(MIN_COLLATERAL_AMOUNT) {
                st.send(
                    &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
                    Method::Register as u64,
                    RawBytes::default(),
                    st.total_stake.clone(),
                )?;
            }
        } else {
            st.send(
                &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
                Method::AddStake as u64,
                RawBytes::default(),
                amount,
            )?;
        }
        st.mutate_state();
        st.save();
        Ok(None)
    }

    fn leave() -> anyhow::Result<Option<RawBytes>> {
        let mut st = State::load();
        let caller = Address::new_id(sdk::message::caller());

        // get stake to know how much to release
        let bt = make_map_with_root::<_, BigIntDe>(&st.stake, &Blockstore)?;
        let stake = get_stake(&bt, &caller.clone())?;
        if stake == TokenAmount::zero() {
            abort!(USR_ILLEGAL_STATE, "caller has no stake in subnet");
        }

        // release from SCA
        if st.status != Status::Terminating {
            st.send(
                &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
                Method::ReleaseStake as u64,
                RawBytes::serialize(FundParams {
                    value: stake.clone(),
                })?,
                TokenAmount::zero(),
            )?;
        }

        // remove stake from balance table
        st.rm_stake(&caller, &stake)?;

        // send back to owner
        st.send(&caller, METHOD_SEND, RawBytes::default(), stake)?;

        st.mutate_state();
        st.save();
        Ok(None)
    }

    fn kill() -> anyhow::Result<Option<RawBytes>> {
        let mut st = State::load();

        if st.status == Status::Terminating || st.status == Status::Killed {
            abort!(
                USR_ILLEGAL_STATE,
                "the subnet is already in a killed or terminating state"
            );
        }
        if st.validator_set.len() != 0 {
            abort!(
                USR_ILLEGAL_STATE,
                "this subnet can only be killed when all validators have left"
            );
        }

        // move to terminating state
        st.status = Status::Terminating;

        // unregister subnet
        st.send(
            &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
            Method::Kill as u64,
            RawBytes::default(),
            TokenAmount::zero(),
        )?;

        st.mutate_state();
        st.save();
        Ok(None)
    }

    /*
    fn leave() -> Option<RawBytes> {
        panic!("not implemented");
    }

    fn kill() -> Option<RawBytes> {
        panic!("not implemented");
    }

    fn submit_checkpoint() -> Option<RawBytes> {
        panic!("not implemented");
    }
    */
}
