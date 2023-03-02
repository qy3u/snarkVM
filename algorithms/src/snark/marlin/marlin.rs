// Copyright (C) 2019-2022 Aleo Systems Inc.
// This file is part of the snarkVM library.

// The snarkVM library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkVM library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkVM library. If not, see <https://www.gnu.org/licenses/>.

use crate::{
    fft::EvaluationDomain,
    polycommit::sonic_pc::{Commitment, Evaluations, LabeledPolynomial, LabeledCommitment, QuerySet, Randomness, SonicKZG10},
    snark::marlin::{
        ahp::{AHPError, AHPForR1CS, indexer::Circuit, EvaluationsProvider},
        proof,
        prover,
        witness_label,
        CircuitInfo,
        CircuitProvingKey,
        CircuitVerifyingKey,
        MarlinError,
        MarlinMode,
        Proof,
        UniversalSRS,
    },
    AlgebraicSponge,
    PrepareOrd,
    SNARKError,
    SNARK,
    SRS,
};
use itertools::Itertools;
use rand::{CryptoRng, Rng};
use snarkvm_curves::PairingEngine;
use snarkvm_fields::{One, PrimeField, ToConstraintField, Zero};
use snarkvm_r1cs::ConstraintSynthesizer;
use snarkvm_utilities::{cfg_iter, to_bytes_le, ToBytes};

use std::{borrow::Borrow, collections::BTreeMap, sync::Arc};
use std::ops::Deref;

#[cfg(not(feature = "std"))]
use snarkvm_utilities::println;

use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use super::Certificate;

/// The Marlin proof system.
#[derive(Clone, Debug)]
pub struct MarlinSNARK<E: PairingEngine, FS: AlgebraicSponge<E::Fq, 2>, MM: MarlinMode>(
    #[doc(hidden)] PhantomData<(E, FS, MM)>,
);

impl<E: PairingEngine, FS: AlgebraicSponge<E::Fq, 2>, MM: MarlinMode> MarlinSNARK<E, FS, MM> {
    /// The personalization string for this protocol.
    /// Used to personalize the Fiat-Shamir RNG.
    pub const PROTOCOL_NAME: &'static [u8] = b"MARLIN-2019";

    /// Generate the index-specific (i.e., circuit-specific) prover and verifier
    /// keys. This is a trusted setup.
    ///
    /// # Warning
    ///
    /// This method should be used *only* for testing purposes, and not in production.
    /// In production, one should instead perform a universal setup via [`Self::universal_setup`],
    /// and then deterministically specialize the resulting universal SRS via [`Self::circuit_setup`].
    #[allow(clippy::type_complexity)]
    pub fn circuit_specific_setup<C: ConstraintSynthesizer<E::Fr>>(
        circuit: &C,
    ) -> Result<(CircuitProvingKey<E, MM>, CircuitVerifyingKey<E, MM>), SNARKError> {
        let indexed_circuit = AHPForR1CS::<E::Fr, MM>::index(circuit)?;
        let srs = Self::universal_setup(&indexed_circuit.max_degree())?;
        return Self::circuit_setup(&srs, circuit);
    }

    pub fn batch_circuit_setup<C: ConstraintSynthesizer<E::Fr>>(
        universal_srs: &UniversalSRS<E>,
        circuits: &[&C],
    ) -> Result<(Vec<CircuitProvingKey<E, MM>>, Vec<CircuitVerifyingKey<E, MM>>), SNARKError> {
        let index_time = start_timer!(|| "Marlin::CircuitSetup");

        let mut circuit_proving_keys = Vec::with_capacity(circuits.len());
        let mut circuit_verifying_keys = Vec::with_capacity(circuits.len());
        for circuit in circuits {
            let indexed_circuit = AHPForR1CS::<_, MM>::index(*circuit)?;
            // TODO: Add check that c is in the correct mode.
            // Increase the universal SRS size to support the circuit size.
            if universal_srs.max_degree() < indexed_circuit.max_degree() {
                universal_srs
                    .download_powers_for(0..indexed_circuit.max_degree())
                    .map_err(|_| MarlinError::IndexTooLarge(universal_srs.max_degree(), indexed_circuit.max_degree()))?;
            }                
            let coefficient_support = AHPForR1CS::<_, MM>::get_degree_bounds(&indexed_circuit.index_info);

            // Marlin only needs degree 2 random polynomials.
            // TODO: investigate whether we can re-use keys across circuits-
            let supported_hiding_bound = 1;
            let (committer_key, verifier_key) = SonicKZG10::<E, FS>::trim(
                universal_srs,
                indexed_circuit.max_degree(),
                [indexed_circuit.constraint_domain_size()],
                supported_hiding_bound,
                Some(coefficient_support.as_slice()),
            )?;

            let commit_time = start_timer!(|| "Commit to index polynomials");
            let (mut circuit_commitments, circuit_commitment_randomness): (_, _) =
                SonicKZG10::<E, FS>::commit(&committer_key, indexed_circuit.iter().map(Into::into), None)?;
            end_timer!(commit_time);

            circuit_commitments.sort_by(|c1, c2| c1.label().cmp(c2.label()));
            let circuit_commitments = circuit_commitments.into_iter().map(|c| *c.commitment()).collect();
            let circuit_verifying_key = CircuitVerifyingKey {
                circuit_info: indexed_circuit.index_info,
                circuit_commitments,
                verifier_key,
                mode: PhantomData,
                hash: format!("{:x?}", indexed_circuit.hash),
            };
            circuit_verifying_keys.push(circuit_verifying_key);

            let circuit_proving_key = CircuitProvingKey {
                circuit: Arc::new(indexed_circuit),
                circuit_commitment_randomness,
                circuit_verifying_key: circuit_verifying_key.clone(),
                committer_key: Arc::new(committer_key),
            };
            circuit_proving_keys.push(circuit_proving_key);
        }

        end_timer!(index_time);

        Ok((circuit_proving_keys, circuit_verifying_keys))

    }

    /// Generates the circuit proving and verifying keys.
    /// This is a deterministic algorithm that anyone can rerun.
    #[allow(clippy::type_complexity)]
    pub fn circuit_setup<C: ConstraintSynthesizer<E::Fr>>(
        universal_srs: &UniversalSRS<E>,
        circuit: &C,
    ) -> Result<(CircuitProvingKey<E, MM>, CircuitVerifyingKey<E, MM>), SNARKError> {
        let (proving_keys, verifying_keys) = Self::batch_circuit_setup(universal_srs, &[circuit])?;
        assert!(proving_keys.len() == 1 && verifying_keys.len() == 1);
        Ok((proving_keys[0], verifying_keys[1]))
    }

    fn terminate(terminator: &AtomicBool) -> Result<(), MarlinError> {
        if terminator.load(Ordering::Relaxed) { Err(MarlinError::Terminated) } else { Ok(()) }
    }

    fn init_sponge<'a>(
        fs_parameters: &FS::Parameters,
        batch_sizes: &BTreeMap<&[u8; 32], (CircuitInfo<E::Fr>, usize)>,
        circuit_commitments: &[Vec<crate::polycommit::sonic_pc::Commitment<E>>],
        inputs: &BTreeMap<&[u8; 32], &Vec<Vec<E::Fr>>>,
    ) -> FS {
        let mut sponge = FS::new_with_parameters(fs_parameters);
        sponge.absorb_bytes(&to_bytes_le![&Self::PROTOCOL_NAME].unwrap());
        for (_, batch_size) in batch_sizes.iter() {
            sponge.absorb_bytes(&batch_size.1.to_le_bytes());
        }
        for circuit_specific_commitments in circuit_commitments {
            sponge.absorb_native_field_elements(circuit_specific_commitments);
        }
        for (_, circuit_specific_input) in inputs {
            for instance_specific_input in circuit_specific_input.iter() {
                sponge.absorb_nonnative_field_elements(instance_specific_input.iter().copied());
            }
        }
        sponge
    }

    fn init_sponge_for_certificate(
        fs_parameters: &FS::Parameters,
        circuit_commitments: &[crate::polycommit::sonic_pc::Commitment<E>],
    ) -> FS {
        let mut sponge = FS::new_with_parameters(fs_parameters);
        sponge.absorb_bytes(&to_bytes_le![&Self::PROTOCOL_NAME].unwrap());
        sponge.absorb_native_field_elements(circuit_commitments);
        sponge
    }

    fn absorb_labeled_with_msg<'a>(
        comms: &[LabeledCommitment<Commitment<E>>],
        message: &prover::ThirdMessage<'a, E::Fr>,
        sponge: &mut FS,
    ) {
        let commitments: Vec<_> = comms.iter().map(|c| *c.commitment()).collect();
        Self::absorb_with_msg(&commitments, message, sponge)
    }

    fn absorb_labeled(comms: &[LabeledCommitment<Commitment<E>>], sponge: &mut FS) {
        let commitments: Vec<_> = comms.iter().map(|c| *c.commitment()).collect();
        Self::absorb(&commitments, sponge);
    }

    fn absorb(commitments: &[Commitment<E>], sponge: &mut FS) {
        let sponge_time = start_timer!(|| "Absorbing commitments");
        sponge.absorb_native_field_elements(commitments);
        end_timer!(sponge_time);
    }

    fn absorb_with_msg<'a>(commitments: &[Commitment<E>], msg: &prover::ThirdMessage<'a, E::Fr>, sponge: &mut FS) {
        let sponge_time = start_timer!(|| "Absorbing commitments and message");
        Self::absorb(commitments, sponge);
        for (_, sum) in msg.sums {
            sponge.absorb_nonnative_field_elements([sum.sum_a, sum.sum_b, sum.sum_c]);
        }
        end_timer!(sponge_time);
    }
}

impl<E: PairingEngine, FS, MM> SNARK for MarlinSNARK<E, FS, MM>
where
    E::Fr: PrimeField,
    E::Fq: PrimeField,
    FS: AlgebraicSponge<E::Fq, 2>,
    MM: MarlinMode,
{
    type BaseField = E::Fq;
    type Certificate = Certificate<E>;
    type FSParameters = FS::Parameters;
    type FiatShamirRng = FS;
    type Proof<'a> = Proof<'a, E>;
    type ProvingKey = CircuitProvingKey<E, MM>;
    type ScalarField = E::Fr;
    type UniversalSetupConfig = usize;
    type UniversalSetupParameters = UniversalSRS<E>;
    type VerifierInput = [E::Fr];
    type VerifyingKey = CircuitVerifyingKey<E, MM>;

    fn universal_setup(max_degree: &Self::UniversalSetupConfig) -> Result<Self::UniversalSetupParameters, SNARKError> {
        let setup_time = start_timer!(|| { format!("Marlin::UniversalSetup with max_degree {}", max_degree,) });

        let srs = SonicKZG10::<E, FS>::load_srs(*max_degree).map_err(Into::into);
        end_timer!(setup_time);
        srs
    }

    fn setup<C: ConstraintSynthesizer<E::Fr>>(
        circuit: &C,
        srs: &mut SRS<Self::UniversalSetupParameters>,
    ) -> Result<(Self::ProvingKey, Self::VerifyingKey), SNARKError> {
        match srs {
            SRS::CircuitSpecific => Self::circuit_specific_setup(circuit),
            SRS::Universal(srs) => Self::circuit_setup(srs, circuit),
        }
        .map_err(SNARKError::from)
    }

    fn prove_vk(
        fs_parameters: &Self::FSParameters,
        verifying_key: &Self::VerifyingKey,
        proving_key: &Self::ProvingKey,
    ) -> Result<Self::Certificate, SNARKError> {
        // Initialize sponge
        let mut sponge = Self::init_sponge_for_certificate(fs_parameters, &verifying_key.circuit_commitments);
        // Compute challenges for linear combination, and the point to evaluate the polynomials at.
        // The linear combination requires `num_polynomials - 1` coefficients
        // (since the first coeff is 1), and so we squeeze out `num_polynomials` points.
        let mut challenges = sponge.squeeze_nonnative_field_elements(verifying_key.circuit_commitments.len());
        let point = challenges.pop().unwrap();
        let one = E::Fr::one();
        let linear_combination_challenges = core::iter::once(&one).chain(challenges.iter());

        // We will construct a linear combination and provide a proof of evaluation of the lc at `point`.
        let mut lc = crate::polycommit::sonic_pc::LinearCombination::empty("circuit_check");
        for (poly, &c) in proving_key.circuit.iter().zip(linear_combination_challenges) {
            lc.add(c, poly.label());
        }

        let query_set = QuerySet::from_iter([("circuit_check".into(), ("challenge".into(), point))]);
        let commitments = verifying_key
            .iter()
            .cloned()
            .zip_eq(AHPForR1CS::<E::Fr, MM>::index_polynomial_info().values())
            .map(|(c, info)| LabeledCommitment::new_with_info(info, c))
            .collect::<Vec<_>>();

        let certificate = SonicKZG10::<E, FS>::open_combinations(
            &proving_key.committer_key,
            &[lc],
            proving_key.circuit.iter(),
            &commitments,
            &query_set,
            &proving_key.circuit_commitment_randomness.clone(),
            &mut sponge,
        )?;

        Ok(Self::Certificate::new(certificate))
    }

    fn verify_vk<C: ConstraintSynthesizer<Self::ScalarField>>(
        fs_parameters: &Self::FSParameters,
        circuit: &C,
        verifying_key: &Self::VerifyingKey,
        certificate: &Self::Certificate,
    ) -> Result<bool, SNARKError> {
        let info = AHPForR1CS::<E::Fr, MM>::index_polynomial_info();
        // Initialize sponge.
        let mut sponge = Self::init_sponge_for_certificate(fs_parameters, &verifying_key.circuit_commitments);
        // Compute challenges for linear combination, and the point to evaluate the polynomials at.
        // The linear combination requires `num_polynomials - 1` coefficients
        // (since the first coeff is 1), and so we squeeze out `num_polynomials` points.
        let mut challenges = sponge.squeeze_nonnative_field_elements(verifying_key.circuit_commitments.len());
        let point = challenges.pop().unwrap();

        let evaluations_at_point = AHPForR1CS::<E::Fr, MM>::evaluate_index_polynomials(circuit, point)?;
        let one = E::Fr::one();
        let linear_combination_challenges = core::iter::once(&one).chain(challenges.iter());

        // We will construct a linear combination and provide a proof of evaluation of the lc at `point`.
        let mut lc = crate::polycommit::sonic_pc::LinearCombination::empty("circuit_check");
        let mut evaluation = E::Fr::zero();
        for ((label, &c), eval) in info.keys().zip_eq(linear_combination_challenges).zip_eq(evaluations_at_point) {
            lc.add(c, label.as_str());
            evaluation += c * eval;
        }

        let query_set = QuerySet::from_iter([("circuit_check".into(), ("challenge".into(), point))]);
        let commitments = verifying_key
            .iter()
            .cloned()
            .zip_eq(info.values())
            .map(|(c, info)| LabeledCommitment::new_with_info(info, c))
            .collect::<Vec<_>>();
        let evaluations = Evaluations::from_iter([(("circuit_check".into(), point), evaluation)]);

        SonicKZG10::<E, FS>::check_combinations(
            &verifying_key.verifier_key,
            &[lc],
            &commitments,
            &query_set,
            &evaluations,
            &certificate.pc_proof,
            &mut sponge,
        )
        .map_err(Into::into)
    }

    #[allow(clippy::only_used_in_recursion)]
    fn prove_batch_with_terminator<'a, C: ConstraintSynthesizer<E::Fr>, R: Rng + CryptoRng>(
        fs_parameters: &Self::FSParameters,
        keys_to_constraints: &BTreeMap<&CircuitProvingKey<E, MM>, &[&C]>,
        terminator: &AtomicBool,
        zk_rng: &mut R,
    ) -> Result<Self::Proof<'a>, SNARKError> {
        let prover_time = start_timer!(|| "Marlin::Prover");
        if keys_to_constraints.len() == 0 {
            return Err(SNARKError::EmptyBatch);
        }

        Self::terminate(terminator)?;

        let circuits_to_constraints = BTreeMap::new();
        for (pk, constraints) in keys_to_constraints {
            circuits_to_constraints.insert(pk.circuit.deref(), *constraints); // TODO: should we pass Arc everywhere instead of a &?
        }
        let prover_state = AHPForR1CS::<_, MM>::init_prover(&circuits_to_constraints)?;
        
        let mut batch_sizes = BTreeMap::new();
        let mut public_inputs = BTreeMap::new();
        let mut padded_public_inputs = BTreeMap::new();
        let mut keys_to_inputs = BTreeMap::new();
        let mut committer_key_union;
        for (pk, _) in keys_to_constraints.iter() {
            let batch_size = prover_state.batch_size(&pk.circuit.hash).ok_or(SNARKError::CircuitNotFound)?;
            let public_input = &prover_state.public_inputs(&pk.circuit.hash).ok_or(SNARKError::CircuitNotFound)?;
            let padded_public_input = &prover_state.padded_public_inputs(&pk.circuit.hash).ok_or(SNARKError::CircuitNotFound)?;
            batch_sizes.insert(&pk.circuit.hash, (pk.circuit_verifying_key.circuit_info, batch_size));
            public_inputs.insert(&pk.circuit.hash, public_input);
            padded_public_inputs.insert(&pk.circuit.hash, padded_public_input);
            committer_key_union = pk.committer_key.clone();
            // public_inputs.insert(&pk.circuit_verifying_key, public_input);
        }
        assert_eq!(prover_state.total_batch_size(), batch_sizes.iter().map(|(_, (_, s))| s).sum::<usize>());

        let circuit_commitments = keys_to_constraints
            .iter()
            .map(|(pk, _)| pk.circuit_verifying_key.circuit_commitments.clone())
            .collect::<Vec<_>>().as_slice();

        let mut sponge = Self::init_sponge(
            fs_parameters,
            &batch_sizes,
            circuit_commitments,
            &padded_public_inputs,
        );

        // --------------------------------------------------------------------
        // First round

        Self::terminate(terminator)?;
        let mut prover_state = AHPForR1CS::<_, MM>::prover_first_round(prover_state, zk_rng)?;
        Self::terminate(terminator)?;

        let first_round_comm_time = start_timer!(|| "Committing to first round polys");
        let (first_commitments, first_commitment_randomnesses) = {
            let first_round_oracles = Arc::get_mut(prover_state.first_round_oracles.as_mut().unwrap()).unwrap();
            SonicKZG10::<E, FS>::commit(
                &keys_to_constraints[0].committer_key, // TODO: take the union of the committer keys
                first_round_oracles.iter_for_commit(),
                Some(zk_rng),
            )?
        };
        end_timer!(first_round_comm_time);

        Self::absorb_labeled(&first_commitments, &mut sponge);
        Self::terminate(terminator)?;

        let (verifier_first_message, verifier_state) = AHPForR1CS::<_, MM>::verifier_first_round(
            &batch_sizes,
            prover_state.max_constraint_domain.clone(),
            prover_state.max_non_zero_domain.clone(),
            &mut sponge,
        )?;
        // --------------------------------------------------------------------

        // --------------------------------------------------------------------
        // Second round

        Self::terminate(terminator)?;
        let (second_oracles, prover_state) =
            AHPForR1CS::<_, MM>::prover_second_round(&verifier_first_message, prover_state, zk_rng);
        Self::terminate(terminator)?;

        let second_round_comm_time = start_timer!(|| "Committing to second round polys");
        let (second_commitments, second_commitment_randomnesses) = SonicKZG10::<E, FS>::commit_with_terminator(
            &keys_to_constraints[0].committer_key, // TODO: take the union of the committer keys
            second_oracles.iter().map(Into::into),
            terminator,
            Some(zk_rng),
        )?;
        end_timer!(second_round_comm_time);

        Self::absorb_labeled(&second_commitments, &mut sponge);
        Self::terminate(terminator)?;

        let (verifier_second_msg, verifier_state) =
            AHPForR1CS::<_, MM>::verifier_second_round(verifier_state, &mut sponge)?;
        // --------------------------------------------------------------------

        // --------------------------------------------------------------------
        // Third round

        Self::terminate(terminator)?;

        let (prover_third_message, third_oracles, prover_state) =
            AHPForR1CS::<_, MM>::prover_third_round(&verifier_second_msg, prover_state, zk_rng)?;
        Self::terminate(terminator)?;

        let third_round_comm_time = start_timer!(|| "Committing to third round polys");
        let (third_commitments, third_commitment_randomnesses) = SonicKZG10::<E, FS>::commit_with_terminator(
            &keys_to_constraints[0].committer_key, // TODO: take the union of the committer keys
            third_oracles.iter().map(Into::into),
            terminator,
            Some(zk_rng),
        )?;
        end_timer!(third_round_comm_time);

        Self::absorb_labeled_with_msg(&third_commitments, &prover_third_message, &mut sponge);

        let (verifier_third_msg, verifier_state) =
            AHPForR1CS::<_, MM>::verifier_third_round(verifier_state, &mut sponge)?;
        // --------------------------------------------------------------------

        // --------------------------------------------------------------------
        // Fourth round

        Self::terminate(terminator)?;

        let first_round_oracles = Arc::clone(prover_state.first_round_oracles.as_ref().unwrap());
        let fourth_oracles = AHPForR1CS::<_, MM>::prover_fourth_round(&verifier_third_msg, prover_state, zk_rng)?;
        Self::terminate(terminator)?;

        let fourth_round_comm_time = start_timer!(|| "Committing to fourth round polys");
        let (fourth_commitments, fourth_commitment_randomnesses) = SonicKZG10::<E, FS>::commit_with_terminator(
            &keys_to_constraints[0].committer_key, // TODO: take the union of the committer keys
            fourth_oracles.iter().map(Into::into),
            terminator,
            Some(zk_rng),
        )?;
        end_timer!(fourth_round_comm_time);

        Self::absorb_labeled(&fourth_commitments, &mut sponge);

        let verifier_state = AHPForR1CS::<_, MM>::verifier_fourth_round(verifier_state, &mut sponge)?;
        // --------------------------------------------------------------------

        Self::terminate(terminator)?;

        // Gather prover polynomials in one vector.
        let polynomials: Vec<_> = keys_to_constraints.iter()
            .flat_map(|(pk, _)|pk.circuit.iter()) // 12 items per circuit
            .chain(first_round_oracles.iter_for_open()) // 3 per instance_batch_size + (MM::ZK as usize) items
            .chain(second_oracles.iter())// 2 items
            .chain(third_oracles.iter())// 3 items per circuit
            .chain(fourth_oracles.iter())// 1 item
            .collect();
        // TODO, somewhere, asserts that out number of oracles matches num_prover_oracles.

        Self::terminate(terminator)?;

        // Gather commitments in one vector.
        let witness_commitments = first_commitments.chunks_exact(3);
        let mask_poly = MM::ZK.then(|| *witness_commitments.remainder()[0].commitment());
        let witness_commitments = witness_commitments
            .map(|c| proof::WitnessCommitments {
                w: *c[0].commitment(),
                z_a: *c[1].commitment(),
                z_b: *c[2].commitment(),
            })
            .collect();
        let third_commitments_chunked = first_commitments.chunks_exact(3);

        #[rustfmt::skip]
        let commitments = proof::Commitments {
            witness_commitments,
            mask_poly,

            g_1: *second_commitments[0].commitment(),
            h_1: *second_commitments[1].commitment(),

            g_a_commitments: third_commitments_chunked.map(|c| *c[0].commitment()).collect(),
            g_b_commitments: third_commitments_chunked.map(|c| *c[1].commitment()).collect(),
            g_c_commitments: third_commitments_chunked.map(|c| *c[2].commitment()).collect(),

            h_2: *fourth_commitments[0].commitment(),
        };

        let labeled_commitments: Vec<_> = keys_to_constraints.iter()
            .flat_map(|(pk, _)|pk.circuit_verifying_key.iter())
            .cloned()
            .zip_eq(AHPForR1CS::<E::Fr, MM>::index_polynomial_info().values())
            .map(|(c, info)| LabeledCommitment::new_with_info(info, c))
            .chain(first_commitments.into_iter())
            .chain(second_commitments.into_iter())
            .chain(third_commitments.into_iter())
            .chain(fourth_commitments.into_iter())
            .collect();

        // Gather commitment randomness together.
        let commitment_randomnesses: Vec<Randomness<E>> = keys_to_constraints.iter()
            .flat_map(|(pk, _)|pk.circuit_commitment_randomness)
            .clone()
            .into_iter()
            .chain(first_commitment_randomnesses)
            .chain(second_commitment_randomnesses)
            .chain(third_commitment_randomnesses)
            .chain(fourth_commitment_randomnesses)
            .collect();

        if !MM::ZK {
            let empty_randomness = Randomness::<E>::empty();
            assert!(commitment_randomnesses.iter().all(|r| r == &empty_randomness));
        }

        // Compute the AHP verifier's query set.
        let (query_set, verifier_state) = AHPForR1CS::<_, MM>::verifier_query_set(verifier_state);
        let lc_s = AHPForR1CS::<_, MM>::construct_linear_combinations(
            &public_inputs,
            &polynomials,
            &prover_third_message,
            &verifier_state,
        )?;

        Self::terminate(terminator)?;

        let eval_time = start_timer!(|| "Evaluating linear combinations over query set");
        let mut evaluations = std::collections::BTreeMap::new();
        for (label, (_, point)) in query_set.to_set() {
            if !AHPForR1CS::<E::Fr, MM>::LC_WITH_ZERO_EVAL.contains(&label.as_str()) {
                let lc = lc_s.get(&label).ok_or_else(|| AHPError::MissingEval(label.to_string()))?;
                let evaluation = polynomials.get_lc_eval(lc, point)?;
                evaluations.insert(label, evaluation);
            }
        }

        let evaluations = proof::Evaluations::from_map(&evaluations, batch_sizes);
        end_timer!(eval_time);

        Self::terminate(terminator)?;

        sponge.absorb_nonnative_field_elements(evaluations.to_field_elements());

        let pc_proof = SonicKZG10::<E, FS>::open_combinations(
            &keys_to_constraints[0].committer_key, // TODO: take the union of the committer keys
            lc_s.values(),
            polynomials,
            &labeled_commitments,
            &query_set.to_set(),
            &commitment_randomnesses,
            &mut sponge,
        )?;

        Self::terminate(terminator)?;

        let proof = Proof::<'a, E>::new(batch_sizes, commitments, evaluations, prover_third_message, pc_proof)?;
        assert_eq!(proof.pc_proof.is_hiding(), MM::ZK);

        #[cfg(debug_assertions)]
        if !Self::verify_batch(fs_parameters, &keys_to_inputs, &proof)? {
            println!("Invalid proof")
        }
        end_timer!(prover_time);

        Ok(proof)
    }

    fn verify_batch_prepared<'a, B: Borrow<Self::VerifierInput>>(
        fs_parameters: &Self::FSParameters,
        keys_to_inputs: &BTreeMap<&<Self::VerifyingKey as PrepareOrd>::Prepared, &[B]>,
        proof: &Self::Proof<'a>,
    ) -> Result<bool, SNARKError> {
        let circuit_verifying_key = &prepared_verifying_key.orig_vk;
        if public_inputs.is_empty() {
            return Err(SNARKError::EmptyBatch);
        }

        if public_inputs.len() != proof.batch_size()? {
            return Err(SNARKError::BatchSizeMismatch);
        }

        let comms = &proof.commitments;
        let proof_has_correct_zk_mode = if MM::ZK {
            proof.pc_proof.is_hiding() & comms.mask_poly.is_some()
        } else {
            !proof.pc_proof.is_hiding() & comms.mask_poly.is_none()
        };
        if !proof_has_correct_zk_mode {
            eprintln!(
                "Found `mask_poly` in the first round when not expected, or proof has incorrect hiding mode ({})",
                proof.pc_proof.is_hiding()
            );
            return Ok(false);
        }

        let batch_size = public_inputs.len();
        let verifier_time = start_timer!(|| format!("Marlin::Verify with batch size {batch_size}"));

        let first_round_info = AHPForR1CS::<E::Fr, MM>::first_round_polynomial_info(batch_size);
        let mut first_commitments = comms
            .witness_commitments
            .iter()
            .enumerate()
            .flat_map(|(i, c)| {
                [
                    LabeledCommitment::new_with_info(&first_round_info[&witness_label("w", i)], c.w),
                    LabeledCommitment::new_with_info(&first_round_info[&witness_label("z_a", i)], c.z_a),
                    LabeledCommitment::new_with_info(&first_round_info[&witness_label("z_b", i)], c.z_b),
                ]
            })
            .collect::<Vec<_>>();
        if MM::ZK {
            first_commitments.push(LabeledCommitment::new_with_info(
                first_round_info.get("mask_poly").unwrap(),
                comms.mask_poly.unwrap(),
            ));
        }

        let second_round_info =
            AHPForR1CS::<E::Fr, MM>::second_round_polynomial_info(&circuit_verifying_key.circuit_info);
        let second_commitments = [
            LabeledCommitment::new_with_info(&second_round_info["g_1"], comms.g_1),
            LabeledCommitment::new_with_info(&second_round_info["h_1"], comms.h_1),
        ];

        let third_round_info =
            AHPForR1CS::<E::Fr, MM>::third_round_polynomial_info(&circuit_verifying_key.circuit_info);
        let third_commitments = [
            LabeledCommitment::new_with_info(&third_round_info["g_a"], comms.g_a),
            LabeledCommitment::new_with_info(&third_round_info["g_b"], comms.g_b),
            LabeledCommitment::new_with_info(&third_round_info["g_c"], comms.g_c),
        ];

        let fourth_round_info = AHPForR1CS::<E::Fr, MM>::fourth_round_polynomial_info();
        let fourth_commitments = [LabeledCommitment::new_with_info(&fourth_round_info["h_2"], comms.h_2)];

        let input_domain =
            EvaluationDomain::<E::Fr>::new(circuit_verifying_key.circuit_info.num_public_inputs).unwrap();

        let (padded_public_inputs, public_inputs): (Vec<_>, Vec<_>) = {
            public_inputs
                .iter()
                .map(|input| {
                    let input = input.borrow().to_field_elements().unwrap();
                    let mut new_input = vec![E::Fr::one()];
                    new_input.extend_from_slice(&input);
                    new_input.resize(input.len().max(input_domain.size()), E::Fr::zero());
                    if cfg!(debug_assertions) {
                        println!("Number of padded public variables: {}", new_input.len());
                    }
                    let unformatted = prover::ConstraintSystem::unformat_public_input(&new_input);
                    (new_input, unformatted)
                })
                .unzip()
        };

        let mut sponge = Self::init_sponge(
            fs_parameters,
            batch_size,
            &circuit_verifying_key.circuit_commitments,
            &padded_public_inputs,
        );

        // --------------------------------------------------------------------
        // First round
        let first_round_time = start_timer!(|| "First round");
        Self::absorb_labeled(&first_commitments, &mut sponge);
        let (_, verifier_state) =
            AHPForR1CS::<_, MM>::verifier_first_round(circuit_verifying_key.circuit_info, batch_size, &mut sponge)?;
        end_timer!(first_round_time);
        // --------------------------------------------------------------------

        // --------------------------------------------------------------------
        // Second round
        let second_round_time = start_timer!(|| "Second round");
        Self::absorb_labeled(&second_commitments, &mut sponge);
        let (_, verifier_state) = AHPForR1CS::<_, MM>::verifier_second_round(verifier_state, &mut sponge)?;
        end_timer!(second_round_time);
        // --------------------------------------------------------------------

        // --------------------------------------------------------------------
        // Third round
        let third_round_time = start_timer!(|| "Third round");

        Self::absorb_labeled_with_msg(&third_commitments, &proof.msg, &mut sponge);
        let (_, verifier_state) = AHPForR1CS::<_, MM>::verifier_third_round(verifier_state, &mut sponge)?;
        end_timer!(third_round_time);
        // --------------------------------------------------------------------

        // --------------------------------------------------------------------
        // Fourth round
        let fourth_round_time = start_timer!(|| "Fourth round");

        Self::absorb_labeled(&fourth_commitments, &mut sponge);
        let verifier_state = AHPForR1CS::<_, MM>::verifier_fourth_round(verifier_state, &mut sponge)?;
        end_timer!(fourth_round_time);
        // --------------------------------------------------------------------

        // Collect degree bounds for commitments. Indexed polynomials have *no*
        // degree bounds because we know the committed index polynomial has the
        // correct degree.

        // Gather commitments in one vector.
        let commitments: Vec<_> = circuit_verifying_key
            .iter()
            .cloned()
            .zip_eq(AHPForR1CS::<E::Fr, MM>::index_polynomial_info().values())
            .map(|(c, info)| LabeledCommitment::new_with_info(info, c))
            .chain(first_commitments)
            .chain(second_commitments)
            .chain(third_commitments)
            .chain(fourth_commitments)
            .collect();

        let query_set_time = start_timer!(|| "Constructing query set");
        let (query_set, verifier_state) = AHPForR1CS::<_, MM>::verifier_query_set(verifier_state);
        end_timer!(query_set_time);

        sponge.absorb_nonnative_field_elements(proof.evaluations.to_field_elements());

        let mut evaluations = Evaluations::new();

        for (label, (_point_name, q)) in query_set.to_set() {
            if AHPForR1CS::<E::Fr, MM>::LC_WITH_ZERO_EVAL.contains(&label.as_ref()) {
                evaluations.insert((label, q), E::Fr::zero());
            } else {
                let eval = proof.evaluations.get(&label).ok_or_else(|| AHPError::MissingEval(label.clone()))?;
                evaluations.insert((label, q), eval);
            }
        }

        let lc_time = start_timer!(|| "Constructing linear combinations");
        let lc_s = AHPForR1CS::<_, MM>::construct_linear_combinations(
            &public_inputs,
            &evaluations,
            &proof.msg,
            &verifier_state,
        )?;
        end_timer!(lc_time);

        let pc_time = start_timer!(|| "Checking linear combinations with PC");
        let evaluations_are_correct = SonicKZG10::<E, FS>::check_combinations(
            &circuit_verifying_key.verifier_key,
            lc_s.values(),
            &commitments,
            &query_set.to_set(),
            &evaluations,
            &proof.pc_proof,
            &mut sponge,
        )?;
        end_timer!(pc_time);

        if !evaluations_are_correct {
            #[cfg(debug_assertions)]
            eprintln!("SonicKZG10::Check failed");
        }
        end_timer!(verifier_time, || format!(
            " SonicKZG10::Check for AHP Verifier linear equations: {}",
            evaluations_are_correct & proof_has_correct_zk_mode
        ));
        Ok(evaluations_are_correct & proof_has_correct_zk_mode)
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::{
        crypto_hash::PoseidonSponge,
        snark::marlin::{MarlinHidingMode, MarlinSNARK},
        AlgebraicSponge,
        SRS,
    };
    use snarkvm_curves::bls12_377::{Bls12_377, Fq, Fr};
    use snarkvm_fields::Field;
    use snarkvm_r1cs::{ConstraintSystem, SynthesisError};
    use snarkvm_utilities::{TestRng, Uniform};

    use core::ops::MulAssign;

    const ITERATIONS: usize = 10;

    #[derive(Copy, Clone)]
    pub struct Circuit<F: Field> {
        pub a: Option<F>,
        pub b: Option<F>,
        pub num_constraints: usize,
        pub num_variables: usize,
    }

    impl<ConstraintF: Field> ConstraintSynthesizer<ConstraintF> for Circuit<ConstraintF> {
        fn generate_constraints<CS: ConstraintSystem<ConstraintF>>(&self, cs: &mut CS) -> Result<(), SynthesisError> {
            let a = cs.alloc(|| "a", || self.a.ok_or(SynthesisError::AssignmentMissing))?;
            let b = cs.alloc(|| "b", || self.b.ok_or(SynthesisError::AssignmentMissing))?;
            let c = cs.alloc_input(
                || "c",
                || {
                    let mut a = self.a.ok_or(SynthesisError::AssignmentMissing)?;
                    let b = self.b.ok_or(SynthesisError::AssignmentMissing)?;

                    a.mul_assign(&b);
                    Ok(a)
                },
            )?;

            for i in 0..(self.num_variables - 3) {
                let _ = cs.alloc(|| format!("var {}", i), || self.a.ok_or(SynthesisError::AssignmentMissing))?;
            }

            for i in 0..(self.num_constraints - 1) {
                cs.enforce(|| format!("constraint {}", i), |lc| lc + a, |lc| lc + b, |lc| lc + c);
            }

            Ok(())
        }
    }

    type FS = PoseidonSponge<Fq, 2, 1>;
    type TestSNARK = MarlinSNARK<Bls12_377, FS, MarlinHidingMode>;

    #[test]
    fn marlin_snark_test() {
        let mut rng = TestRng::default();

        for _ in 0..ITERATIONS {
            // Construct the circuit.

            let a = Fr::rand(&mut rng);
            let b = Fr::rand(&mut rng);
            let mut c = a;
            c.mul_assign(&b);

            let circ = Circuit { a: Some(a), b: Some(b), num_constraints: 100, num_variables: 25 };

            // Generate the circuit parameters.

            let (pk, vk) = TestSNARK::setup(&circ, &mut SRS::CircuitSpecific).unwrap();

            // Test native proof and verification.
            let fs_parameters = FS::sample_parameters();

            let proof = TestSNARK::prove(&fs_parameters, &pk, &circ, &mut rng).unwrap();

            assert!(
                TestSNARK::verify(&fs_parameters, &vk.clone(), [c].as_ref(), &proof).unwrap(),
                "The native verification check fails."
            );
        }
    }
}
