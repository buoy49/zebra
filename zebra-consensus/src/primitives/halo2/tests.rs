//! Tests for verifying simple Halo2 proofs with the async verifier

use std::convert::TryInto;

use futures::stream::{FuturesUnordered, StreamExt};
use tower::ServiceExt;

use halo2::{arithmetic::FieldExt, pasta::pallas};
use orchard::{
    builder::Builder,
    bundle::Flags,
    circuit::ProvingKey,
    keys::{FullViewingKey, SpendingKey},
    value::NoteValue,
    Anchor, Bundle,
};
use rand::rngs::OsRng;

use zebra_chain::orchard::ShieldedData;

use crate::primitives::halo2::*;

lazy_static::lazy_static! {
    pub static ref PROVING_KEY: ProvingKey = ProvingKey::build();
}

async fn verify_orchard_halo2_proofs<V>(
    verifier: &mut V,
    shielded_data: Vec<ShieldedData>,
) -> Result<(), V::Error>
where
    V: tower::Service<Item, Response = ()>,
    <V as tower::Service<Item>>::Error: std::convert::From<
        std::boxed::Box<dyn std::error::Error + std::marker::Send + std::marker::Sync>,
    >,
    <V as tower::Service<Item>>::Error: std::fmt::Debug,
{
    let mut async_checks = FuturesUnordered::new();

    for sd in shielded_data {
        tracing::trace!(?sd);

        let rsp = verifier.ready().await?.call(Item::from(sd));

        async_checks.push(rsp);
    }

    while let Some(result) = async_checks.next().await {
        tracing::trace!(?result);
        result?;
    }

    Ok(())
}

#[tokio::test]
async fn verify_generated_halo2_proofs() {
    zebra_test::init();

    let rng = OsRng;

    let sk = SpendingKey::from_bytes([7; 32]).unwrap();
    let recipient = FullViewingKey::from(&sk).default_address();

    let enable_spends = true;
    let enable_outputs = true;
    let flags =
        zebra_chain::orchard::Flags::ENABLE_SPENDS | zebra_chain::orchard::Flags::ENABLE_OUTPUTS;

    let anchor_bytes = [0; 32];
    let note_value = 10;

    let shielded_data: Vec<zebra_chain::orchard::ShieldedData> = (1..=4)
        .map(|num_recipients| {
            let mut builder = Builder::new(
                Flags::from_parts(enable_spends, enable_outputs),
                Anchor::from_bytes(anchor_bytes).unwrap(),
            );

            for _ in 0..num_recipients {
                builder
                    .add_recipient(None, recipient, NoteValue::from_raw(note_value), None)
                    .unwrap();
            }

            let bundle: Bundle<_, i64> = builder.build(rng).unwrap();

            let bundle = bundle
                .create_proof(&PROVING_KEY)
                .unwrap()
                .apply_signatures(rng, [0; 32], &[])
                .unwrap();

            zebra_chain::orchard::ShieldedData {
                flags,
                value_balance: note_value.try_into().unwrap(),
                shared_anchor: anchor_bytes.try_into().unwrap(),
                proof: zebra_chain::primitives::Halo2Proof(
                    bundle.authorization().proof().as_ref().into(),
                ),
                actions: bundle
                    .actions()
                    .iter()
                    .map(|a| {
                        let action = zebra_chain::orchard::Action {
                            cv: a.cv_net().to_bytes().try_into().unwrap(),
                            nullifier: a.nullifier().to_bytes().try_into().unwrap(),
                            rk: <[u8; 32]>::from(a.rk()).try_into().unwrap(),
                            cm_x: pallas::Base::from_bytes(&a.cmx().into()).unwrap(),
                            ephemeral_key: a.encrypted_note().epk_bytes.try_into().unwrap(),
                            enc_ciphertext: a.encrypted_note().enc_ciphertext.into(),
                            out_ciphertext: a.encrypted_note().out_ciphertext.into(),
                        };
                        zebra_chain::orchard::shielded_data::AuthorizedAction {
                            action,
                            spend_auth_sig: <[u8; 64]>::from(a.authorization()).into(),
                        }
                    })
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap(),
                binding_sig: <[u8; 64]>::from(bundle.authorization().binding_signature())
                    .try_into()
                    .unwrap(),
            }
        })
        .collect();

    // Use separate verifier so shared batch tasks aren't killed when the test ends (#2390)
    let mut verifier = Fallback::new(
        Batch::new(
            Verifier::new(&VERIFYING_KEY),
            crate::primitives::MAX_BATCH_SIZE,
            crate::primitives::MAX_BATCH_LATENCY,
        ),
        tower::service_fn(
            (|item: Item| ready(item.verify_single(&VERIFYING_KEY).map_err(Halo2Error::from)))
                as fn(_) -> _,
        ),
    );

    // This should fail if any of the proofs fail to validate.
    verify_orchard_halo2_proofs(&mut verifier, shielded_data)
        .await
        .unwrap()
}