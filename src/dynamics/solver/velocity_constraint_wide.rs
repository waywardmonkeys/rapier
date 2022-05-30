use super::{
    AnyVelocityConstraint, DeltaVel, VelocityConstraintElement, VelocityConstraintNormalPart,
};
use crate::dynamics::{
    IntegrationParameters, RigidBodyIds, RigidBodyMassProps, RigidBodySet, RigidBodyVelocity,
};
use crate::geometry::{ContactManifold, ContactManifoldIndex};
use crate::math::{
    AngVector, AngularInertia, Point, Real, SimdReal, Vector, DIM, MAX_MANIFOLD_POINTS, SIMD_WIDTH,
};
#[cfg(feature = "dim2")]
use crate::utils::WBasis;
use crate::utils::{self, WAngularInertia, WCross, WDot};
use num::Zero;
use parry::math::SimdBool;
use simba::simd::{SimdPartialOrd, SimdValue};

#[derive(Copy, Clone, Debug)]
pub(crate) struct WVelocityConstraint {
    pub dir1: Vector<SimdReal>, // Non-penetration force direction for the first body.
    #[cfg(feature = "dim3")]
    pub tangent1: Vector<SimdReal>, // One of the friction force directions.
    pub elements: [VelocityConstraintElement<SimdReal>; MAX_MANIFOLD_POINTS],
    pub num_contacts: u8,
    pub im1: Vector<SimdReal>,
    pub im2: Vector<SimdReal>,
    pub cfm_factor: SimdReal,
    pub limit: SimdReal,
    pub mj_lambda1: [usize; SIMD_WIDTH],
    pub mj_lambda2: [usize; SIMD_WIDTH],
    pub manifold_id: [ContactManifoldIndex; SIMD_WIDTH],
    pub manifold_contact_id: [[u8; SIMD_WIDTH]; MAX_MANIFOLD_POINTS],
}

impl WVelocityConstraint {
    pub fn generate(
        params: &IntegrationParameters,
        manifold_id: [ContactManifoldIndex; SIMD_WIDTH],
        manifolds: [&ContactManifold; SIMD_WIDTH],
        bodies: &RigidBodySet,
        out_constraints: &mut Vec<AnyVelocityConstraint>,
        insert_at: Option<usize>,
    ) {
        for ii in 0..SIMD_WIDTH {
            assert_eq!(manifolds[ii].data.relative_dominance, 0);
        }

        let cfm_factor = SimdReal::splat(params.cfm_factor());
        let dt = SimdReal::splat(params.dt);
        let inv_dt = SimdReal::splat(params.inv_dt());
        let allowed_lin_err = SimdReal::splat(params.allowed_linear_error);
        let erp_inv_dt = SimdReal::splat(params.erp_inv_dt());
        let max_penetration_correction = SimdReal::splat(params.max_penetration_correction);

        let handles1 = gather![|ii| manifolds[ii].data.rigid_body1.unwrap()];
        let handles2 = gather![|ii| manifolds[ii].data.rigid_body2.unwrap()];

        let vels1: [&RigidBodyVelocity; SIMD_WIDTH] = gather![|ii| &bodies[handles1[ii]].vels];
        let vels2: [&RigidBodyVelocity; SIMD_WIDTH] = gather![|ii| &bodies[handles2[ii]].vels];
        let ids1: [&RigidBodyIds; SIMD_WIDTH] = gather![|ii| &bodies[handles1[ii]].ids];
        let ids2: [&RigidBodyIds; SIMD_WIDTH] = gather![|ii| &bodies[handles2[ii]].ids];
        let mprops1: [&RigidBodyMassProps; SIMD_WIDTH] = gather![|ii| &bodies[handles1[ii]].mprops];
        let mprops2: [&RigidBodyMassProps; SIMD_WIDTH] = gather![|ii| &bodies[handles2[ii]].mprops];

        let ccd_thickness1 = SimdReal::from(gather![|ii| bodies[handles1[ii]].ccd.ccd_thickness]);
        let ccd_thickness2 = SimdReal::from(gather![|ii| bodies[handles2[ii]].ccd.ccd_thickness]);
        let ccd_thickness = ccd_thickness1 + ccd_thickness2;

        let world_com1 = Point::from(gather![|ii| mprops1[ii].world_com]);
        let im1 = Vector::from(gather![|ii| mprops1[ii].effective_inv_mass]);
        let ii1: AngularInertia<SimdReal> =
            AngularInertia::from(gather![|ii| mprops1[ii].effective_world_inv_inertia_sqrt]);

        let linvel1 = Vector::from(gather![|ii| vels1[ii].linvel]);
        let angvel1 = AngVector::<SimdReal>::from(gather![|ii| vels1[ii].angvel]);

        let world_com2 = Point::from(gather![|ii| mprops2[ii].world_com]);
        let im2 = Vector::from(gather![|ii| mprops2[ii].effective_inv_mass]);
        let ii2: AngularInertia<SimdReal> =
            AngularInertia::from(gather![|ii| mprops2[ii].effective_world_inv_inertia_sqrt]);

        let linvel2 = Vector::from(gather![|ii| vels2[ii].linvel]);
        let angvel2 = AngVector::<SimdReal>::from(gather![|ii| vels2[ii].angvel]);

        let force_dir1 = -Vector::from(gather![|ii| manifolds[ii].data.normal]);

        let mj_lambda1 = gather![|ii| ids1[ii].active_set_offset];
        let mj_lambda2 = gather![|ii| ids2[ii].active_set_offset];

        let num_active_contacts = manifolds[0].data.num_active_contacts();

        #[cfg(feature = "dim2")]
        let tangents1 = force_dir1.orthonormal_basis();
        #[cfg(feature = "dim3")]
        let tangents1 = super::compute_tangent_contact_directions(&force_dir1, &linvel1, &linvel2);

        for l in (0..num_active_contacts).step_by(MAX_MANIFOLD_POINTS) {
            let manifold_points =
                gather![|ii| &manifolds[ii].data.solver_contacts[l..num_active_contacts]];
            let num_points = manifold_points[0].len().min(MAX_MANIFOLD_POINTS);

            let mut is_fast_contact = SimdBool::splat(false);
            let mut constraint = WVelocityConstraint {
                dir1: force_dir1,
                #[cfg(feature = "dim3")]
                tangent1: tangents1[0],
                elements: [VelocityConstraintElement::zero(); MAX_MANIFOLD_POINTS],
                im1,
                im2,
                cfm_factor,
                limit: SimdReal::splat(0.0),
                mj_lambda1,
                mj_lambda2,
                manifold_id,
                manifold_contact_id: [[0; SIMD_WIDTH]; MAX_MANIFOLD_POINTS],
                num_contacts: num_points as u8,
            };

            for k in 0..num_points {
                let friction = SimdReal::from(gather![|ii| manifold_points[ii][k].friction]);
                let restitution = SimdReal::from(gather![|ii| manifold_points[ii][k].restitution]);
                let is_bouncy = SimdReal::from(gather![
                    |ii| manifold_points[ii][k].is_bouncy() as u32 as Real
                ]);
                let is_resting = SimdReal::splat(1.0) - is_bouncy;
                let point = Point::from(gather![|ii| manifold_points[ii][k].point]);
                let dist = SimdReal::from(gather![|ii| manifold_points[ii][k].dist]);
                let tangent_velocity =
                    Vector::from(gather![|ii| manifold_points[ii][k].tangent_velocity]);
                let dp1 = point - world_com1;
                let dp2 = point - world_com2;

                let vel1 = linvel1 + angvel1.gcross(dp1);
                let vel2 = linvel2 + angvel2.gcross(dp2);

                constraint.limit = friction;
                constraint.manifold_contact_id[k] = gather![|ii| manifold_points[ii][k].contact_id];

                // Normal part.
                {
                    let gcross1 = ii1.transform_vector(dp1.gcross(force_dir1));
                    let gcross2 = ii2.transform_vector(dp2.gcross(-force_dir1));

                    let imsum = im1 + im2;
                    let projected_mass = utils::simd_inv(
                        force_dir1.dot(&imsum.component_mul(&force_dir1))
                            + gcross1.gdot(gcross1)
                            + gcross2.gdot(gcross2),
                    );

                    let projected_velocity = (vel1 - vel2).dot(&force_dir1);
                    let mut rhs_wo_bias =
                        (SimdReal::splat(1.0) + is_bouncy * restitution) * projected_velocity;
                    rhs_wo_bias += dist.simd_max(SimdReal::zero()) * inv_dt;
                    rhs_wo_bias *= is_bouncy + is_resting;
                    let rhs_bias = (dist + allowed_lin_err)
                        .simd_clamp(-max_penetration_correction, SimdReal::zero())
                        * (erp_inv_dt/* * is_resting */);

                    let rhs = rhs_wo_bias + rhs_bias;
                    is_fast_contact =
                        is_fast_contact | (-rhs * dt).simd_gt(ccd_thickness * SimdReal::splat(0.5));

                    constraint.elements[k].normal_part = VelocityConstraintNormalPart {
                        gcross1,
                        gcross2,
                        rhs: rhs_wo_bias + rhs_bias,
                        rhs_wo_bias,
                        impulse: SimdReal::splat(0.0),
                        r: projected_mass,
                    };
                }

                // tangent parts.
                constraint.elements[k].tangent_part.impulse = na::zero();

                for j in 0..DIM - 1 {
                    let gcross1 = ii1.transform_vector(dp1.gcross(tangents1[j]));
                    let gcross2 = ii2.transform_vector(dp2.gcross(-tangents1[j]));
                    let imsum = im1 + im2;
                    let r = tangents1[j].dot(&imsum.component_mul(&tangents1[j]))
                        + gcross1.gdot(gcross1)
                        + gcross2.gdot(gcross2);
                    let rhs = (vel1 - vel2 + tangent_velocity).dot(&tangents1[j]);

                    constraint.elements[k].tangent_part.gcross1[j] = gcross1;
                    constraint.elements[k].tangent_part.gcross2[j] = gcross2;
                    constraint.elements[k].tangent_part.rhs[j] = rhs;
                    constraint.elements[k].tangent_part.r[j] = if cfg!(feature = "dim2") {
                        utils::simd_inv(r)
                    } else {
                        r
                    };
                }

                #[cfg(feature = "dim3")]
                {
                    constraint.elements[k].tangent_part.r[2] = SimdReal::splat(2.0)
                        * (constraint.elements[k].tangent_part.gcross1[0]
                            .gdot(constraint.elements[k].tangent_part.gcross1[1])
                            + constraint.elements[k].tangent_part.gcross2[0]
                                .gdot(constraint.elements[k].tangent_part.gcross2[1]));
                }
            }

            constraint.cfm_factor = SimdReal::splat(1.0).select(is_fast_contact, cfm_factor);

            if let Some(at) = insert_at {
                out_constraints[at + l / MAX_MANIFOLD_POINTS] =
                    AnyVelocityConstraint::Grouped(constraint);
            } else {
                out_constraints.push(AnyVelocityConstraint::Grouped(constraint));
            }
        }
    }

    pub fn solve(
        &mut self,
        mj_lambdas: &mut [DeltaVel<Real>],
        solve_normal: bool,
        solve_friction: bool,
    ) {
        let mut mj_lambda1 = DeltaVel {
            linear: Vector::from(gather![|ii| mj_lambdas[self.mj_lambda1[ii] as usize].linear]),
            angular: AngVector::from(gather![
                |ii| mj_lambdas[self.mj_lambda1[ii] as usize].angular
            ]),
        };

        let mut mj_lambda2 = DeltaVel {
            linear: Vector::from(gather![|ii| mj_lambdas[self.mj_lambda2[ii] as usize].linear]),
            angular: AngVector::from(gather![
                |ii| mj_lambdas[self.mj_lambda2[ii] as usize].angular
            ]),
        };

        VelocityConstraintElement::solve_group(
            self.cfm_factor,
            &mut self.elements[..self.num_contacts as usize],
            &self.dir1,
            #[cfg(feature = "dim3")]
            &self.tangent1,
            &self.im1,
            &self.im2,
            self.limit,
            &mut mj_lambda1,
            &mut mj_lambda2,
            solve_normal,
            solve_friction,
        );

        for ii in 0..SIMD_WIDTH {
            mj_lambdas[self.mj_lambda1[ii] as usize].linear = mj_lambda1.linear.extract(ii);
            mj_lambdas[self.mj_lambda1[ii] as usize].angular = mj_lambda1.angular.extract(ii);
        }
        for ii in 0..SIMD_WIDTH {
            mj_lambdas[self.mj_lambda2[ii] as usize].linear = mj_lambda2.linear.extract(ii);
            mj_lambdas[self.mj_lambda2[ii] as usize].angular = mj_lambda2.angular.extract(ii);
        }
    }

    pub fn writeback_impulses(&self, manifolds_all: &mut [&mut ContactManifold]) {
        for k in 0..self.num_contacts as usize {
            let impulses: [_; SIMD_WIDTH] = self.elements[k].normal_part.impulse.into();
            #[cfg(feature = "dim2")]
            let tangent_impulses: [_; SIMD_WIDTH] = self.elements[k].tangent_part.impulse[0].into();
            #[cfg(feature = "dim3")]
            let tangent_impulses = self.elements[k].tangent_part.impulse;

            for ii in 0..SIMD_WIDTH {
                let manifold = &mut manifolds_all[self.manifold_id[ii]];
                let contact_id = self.manifold_contact_id[k][ii];
                let active_contact = &mut manifold.points[contact_id as usize];
                active_contact.data.impulse = impulses[ii];

                #[cfg(feature = "dim2")]
                {
                    active_contact.data.tangent_impulse = tangent_impulses[ii];
                }
                #[cfg(feature = "dim3")]
                {
                    active_contact.data.tangent_impulse = tangent_impulses.extract(ii);
                }
            }
        }
    }

    pub fn remove_cfm_and_bias_from_rhs(&mut self) {
        self.cfm_factor = SimdReal::splat(1.0);
        for elt in &mut self.elements {
            elt.normal_part.rhs = elt.normal_part.rhs_wo_bias;
        }
    }
}
