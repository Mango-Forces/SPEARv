//! One canonical identity owner for a sector recipe.
//!
//! The fingerprint covers the exact admitted region, the common grid, the
//! reduction recipe versions and every member's canonical identity. Sector
//! name/ID, editor revision, timeline order, runtime handles and display
//! stretch are deliberately excluded. Execution order is canonicalized with the
//! same member identity so a cache key always names the same floating-point
//! summation order.
use crate::types::{ObservationRegion, TILE_SIZE};
use crate::SnippetPlan;
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// Bump when the numerical recipe changes even if its description string does not.
pub(crate) const RECIPE_VERSION: &str = "spearv-sector-reduction-v1";
/// Bump when the fingerprint input encoding changes.
pub(crate) const PROVENANCE_VERSION: u32 = 1;

fn push_bytes(output: &mut Vec<u8>, bytes: &[u8]) {
    output.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    output.extend_from_slice(bytes);
}

/// Canonical member bytes. The internal band is the primary sort key because
/// execution is grouped and summed per band.
fn identity(plan: &SnippetPlan) -> Vec<u8> {
    let mut output = Vec::new();
    output.extend_from_slice(&plan.support.band.to_le_bytes());
    push_bytes(&mut output, plan.row.id.as_bytes());
    push_bytes(&mut output, plan.row.snippet_ref.as_bytes());
    push_bytes(&mut output, plan.file.key.as_bytes());
    push_bytes(&mut output, plan.row.version.as_bytes());
    push_bytes(&mut output, plan.file.version.as_bytes());
    match &plan.file.etag {
        Some(etag) => {
            output.push(1);
            push_bytes(&mut output, etag.as_bytes());
        }
        None => output.push(0),
    }
    push_bytes(&mut output, plan.row.release.as_bytes());
    output.extend_from_slice(&plan.row.mjd.to_bits().to_le_bytes());
    output.extend_from_slice(&plan.file.mjd_avg.to_bits().to_le_bytes());
    output.extend_from_slice(&plan.row.detector.to_le_bytes());
    push_bytes(&mut output, plan.row.obsid.as_bytes());
    output.extend_from_slice(&plan.support.count.to_le_bytes());
    for value in [plan.support.wl_min, plan.support.wl_max] {
        output.extend_from_slice(&value.to_bits().to_le_bytes());
    }
    let window = plan.support.window.clone();
    for value in [window.x0, window.y0, window.width, window.height] {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output.push(u8::from(window.clipped));
    for value in plan.support.grid_bounds {
        output.extend_from_slice(&value.to_le_bytes());
    }
    output
}

/// Sort reference members by the same canonical identity the fingerprint uses.
pub(crate) fn canonicalize(members: &mut [&Arc<SnippetPlan>]) {
    members.sort_by_cached_key(|plan| identity(plan));
}

/// SHA-256 over the recipe version, exact region bits, common grid and the
/// canonically ordered member identities.
pub(crate) fn fingerprint(region: &ObservationRegion, members: &[Arc<SnippetPlan>]) -> String {
    fingerprint_with_version(region, members, RECIPE_VERSION)
}

fn fingerprint_with_version(
    region: &ObservationRegion,
    members: &[Arc<SnippetPlan>],
    recipe_version: &str,
) -> String {
    let mut identities: Vec<_> = members.iter().map(|plan| identity(plan)).collect();
    identities.sort();
    let grid = region.grid();
    let mut digest = Sha256::new();
    digest.update(b"spearv-sector-fingerprint\0");
    let mut recipe = Vec::new();
    push_bytes(&mut recipe, recipe_version.as_bytes());
    digest.update(recipe);
    digest.update(PROVENANCE_VERSION.to_le_bytes());
    let (fov_l, fov_h) = region.axes();
    for value in [region.ra, region.dec, fov_l, fov_h] {
        digest.update(value.to_bits().to_le_bytes());
    }
    digest.update(grid.width.to_le_bytes());
    digest.update(grid.height.to_le_bytes());
    digest.update(TILE_SIZE.to_le_bytes());
    digest.update((identities.len() as u64).to_le_bytes());
    for identity in identities {
        digest.update((identity.len() as u64).to_le_bytes());
        digest.update(identity);
    }
    hex::encode(digest.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sector::tests::{plan, region};

    fn with_plan(plan: &SnippetPlan, change: impl FnOnce(&mut SnippetPlan)) -> Arc<SnippetPlan> {
        let mut copy = SnippetPlan {
            region: plan.region.clone(),
            file: plan.file.clone(),
            support: plan.support.clone(),
            row: plan.row.clone(),
        };
        change(&mut copy);
        Arc::new(copy)
    }

    #[test]
    fn fingerprint_is_order_independent_and_public_order_is_not_used() {
        let a = plan("a", 0, 2.0, region());
        let b = plan("b", 1, 1.0, region());
        let c = plan("c", 0, 3.0, region());
        let forward = vec![a.clone(), b.clone(), c.clone()];
        let reordered = vec![c, a, b];
        assert_eq!(
            fingerprint(&region(), &forward),
            fingerprint(&region(), &reordered),
            "submitted order must not change the recipe identity"
        );
        let mut left: Vec<_> = forward.iter().collect();
        let mut right: Vec<_> = reordered.iter().collect();
        canonicalize(&mut left);
        canonicalize(&mut right);
        let ids = |members: &[&Arc<SnippetPlan>]| {
            members
                .iter()
                .map(|plan| plan.row.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&left), ids(&right));
    }

    #[test]
    fn member_identity_fields_each_invalidate_the_fingerprint() {
        let a = plan("a", 0, 2.0, region());
        let b = plan("b", 1, 1.0, region());
        let base = fingerprint(&region(), &[a.clone(), b.clone()]);
        let changed = [
            with_plan(&a, |plan| plan.row.version = "v2".into()),
            with_plan(&a, |plan| plan.file.version = "v2".into()),
            with_plan(&a, |plan| plan.file.etag = None),
            with_plan(&a, |plan| {
                plan.support.band = 2;
                plan.row.band = 3;
            }),
            with_plan(&a, |plan| {
                plan.support.window.x0 += 1;
            }),
            with_plan(&a, |plan| {
                plan.support.grid_bounds[0] += 1;
            }),
        ];
        for member in changed {
            assert_ne!(
                fingerprint(&region(), &[member, b.clone()]),
                base,
                "a changed member identity must miss the cache"
            );
        }
        assert_ne!(
            fingerprint(&region(), std::slice::from_ref(&a)),
            base,
            "a different member set must miss the cache"
        );
        let other_region = ObservationRegion {
            fov: region().fov + 1.0,
            ..region()
        };
        assert_ne!(
            fingerprint(&other_region, &[a.clone(), b.clone()]),
            base,
            "a changed admitted region must miss the cache"
        );
        assert_ne!(
            fingerprint_with_version(&region(), &[a, b], "recipe-v2"),
            base,
            "a changed recipe version must miss the cache"
        );
    }
}
