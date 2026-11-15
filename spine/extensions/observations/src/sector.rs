//! One owner for a sector build on the common observation grid.
//!
//! A sector is reduced from its exact member snippets with the approved rules
//! (`reduction`). Calibrated float frames and sparse band counts are kept; no
//! PNG is ever averaged, stretched or used as an input. The fingerprint covers
//! only the render identity, so renaming or reordering the editor node reuses
//! the same numerical result.
use crate::artifact::{self, Artifact};
use crate::counts::CountWriter;
use crate::provenance;
use crate::reduction::{grid_pixel_rect, BandAccumulator, Measurement, SectorBuilder, SectorImage};
use crate::types::{
    mjd_utc, BandContribution, MemberRef, ObservationRegion, SectorBuildInfo, SectorFrameInfo,
    SequenceSector,
};
use crate::SnippetPlan;
use spearv_core::{sample_at, Cutout, Input, TanGrid};
use spearv_spherex::SpherexSource;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const REDUCTION: &str = "IMAGE − ZODI; strict Spectrum mask; inverse-variance mean within each band; equal-weight mean across the sector's bands; pixel is no-data unless every band is present; nearest-pixel sampling; native release calibration.";

/// Locking rule: the `Mutex<Build>` guards only the small status state. Nobody
/// holds it across disk I/O, a full-frame scan or artifact creation. The builder
/// re-locks only to publish a result.
pub struct Build {
    pub info: SectorBuildInfo,
    pub task: Option<tokio::task::AbortHandle>,
    /// Set when the build is released or expires. Work finishing after that is
    /// discarded instead of published.
    pub released: bool,
}

impl Build {
    pub fn new(info: SectorBuildInfo) -> Self {
        Self {
            info,
            task: None,
            released: false,
        }
    }
}

pub struct Admitted {
    pub region: ObservationRegion,
    pub sector: SequenceSector,
    pub members: Vec<Arc<SnippetPlan>>,
}

/// Why a sector build was refused. An unknown (expired or never issued) snippet
/// reference is a 404; every other refusal is a 400.
#[derive(Debug)]
pub enum Refusal {
    Invalid(String),
    UnknownSnippet(String),
}

impl Refusal {
    #[cfg(test)]
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(message) | Self::UnknownSnippet(message) => message,
        }
    }
}

/// Nothing is deduplicated, moved or dropped. The submitted sector order is
/// retained for public metadata; execution order is canonicalized later.
pub fn admit(
    region: &ObservationRegion,
    sectors: Vec<SequenceSector>,
    lookup: impl Fn(&str) -> Option<Arc<SnippetPlan>>,
) -> Result<Vec<Admitted>, Refusal> {
    region.validate().map_err(Refusal::Invalid)?;
    if sectors.is_empty() {
        return Err(Refusal::Invalid("Add at least one sector".into()));
    }
    let mut ids = BTreeSet::new();
    let mut owners: HashMap<String, String> = HashMap::new();
    let mut admitted = Vec::new();
    for sector in sectors {
        if sector.id.trim().is_empty() || !ids.insert(sector.id.clone()) {
            return Err(Refusal::Invalid(format!(
                "Sector id '{}' is empty or repeated",
                sector.id
            )));
        }
        if sector.snippets.is_empty() {
            return Err(Refusal::Invalid(format!(
                "Sector '{}' is empty; remove it or join snippets before building",
                sector.id
            )));
        }
        let mut members = Vec::new();
        for reference in &sector.snippets {
            if let Some(owner) = owners.insert(reference.clone(), sector.id.clone()) {
                return Err(Refusal::Invalid(format!(
                    "Snippet {reference} appears in sector '{owner}' and sector '{}'; each snippet belongs to exactly one sector",
                    sector.id
                )));
            }
            let plan = lookup(reference).ok_or_else(|| {
                Refusal::UnknownSnippet(format!(
                    "Unknown snippet {reference} in sector '{}'; refresh discovery",
                    sector.id
                ))
            })?;
            if !region.same_grid(&plan.region) {
                return Err(Refusal::Invalid(format!(
                    "Snippet {reference} belongs to a different region"
                )));
            }
            members.push(plan);
        }
        admitted.push(Admitted {
            region: region.clone(),
            sector,
            members,
        });
    }
    Ok(admitted)
}

pub fn frame_info(admitted: &Admitted) -> Result<SectorFrameInfo, String> {
    let mjds = admitted.members.iter().map(|plan| plan.row.mjd);
    let start = mjds.clone().fold(f64::INFINITY, f64::min);
    let end = mjds.fold(f64::NEG_INFINITY, f64::max);
    let bands: BTreeSet<u16> = admitted.members.iter().map(|plan| plan.row.band).collect();
    let releases: BTreeSet<String> = admitted
        .members
        .iter()
        .map(|plan| plan.row.release.clone())
        .collect();
    let members = admitted
        .members
        .iter()
        .map(|plan| MemberRef {
            snippet: plan.row.clone(),
            s3_key: plan.file.key.clone(),
            etag: plan.file.etag.clone(),
            region: plan.region.clone(),
        })
        .collect();
    let fingerprint = provenance::fingerprint(&admitted.region, &admitted.members);
    Ok(SectorFrameInfo {
        id: admitted.sector.id.clone(),
        revision: admitted.sector.revision,
        state: "queued".into(),
        error: None,
        snippets: admitted.members.len(),
        members,
        fingerprint,
        bands: bands.into_iter().collect(),
        releases: releases.into_iter().collect(),
        mjd_start: start,
        mjd_end: end,
        utc_start: mjd_utc(start)?,
        utc_end: mjd_utc(end)?,
        complete_pixels: 0,
        target_coverage: 0.0,
        target_covered: false,
        contributions: None,
        count_bytes: None,
        bytes_read: 0,
        requests: 0,
        data_tiles: Vec::new(),
    })
}

/// Map one member cutout onto the grid in PNG order and accumulate its band.
/// Masking is left to the reducer so the approved strict policy is the only one.
pub fn sample_member(
    cutout: &Cutout,
    grid: &TanGrid,
    accumulator: &mut BandAccumulator,
    bounds: Option<[u32; 4]>,
) -> Result<(), String> {
    let zodi = cutout
        .zodi
        .as_ref()
        .ok_or("Sector build requires the ZODI plane")?;
    let input = Input {
        wcs: &cutout.wcs,
        image: &cutout.image.data,
        variance: &cutout.variance.data,
        flags: &cutout.flags.data,
        zodi: &zodi.data,
        wavelength: &cutout.wavelength,
        origin_fits: cutout.origin_fits,
        width: cutout.image.width as usize,
        flag_bits: cutout.flag_bits,
        mask: false,
        ignore_outlier: false,
    };
    let band = accumulator.band();
    // Bounds come from the exhaustive discovery scan, not a corner approximation.
    // Skipped pixels provably have no geometric support for this exact snippet.
    let [left, right, bottom, top] = bounds.unwrap_or([0, grid.width - 1, 0, grid.height - 1]);
    for png_y in grid.height - 1 - top..=grid.height - 1 - bottom {
        for x in left..=right {
            let (ra, dec) = grid.sky(f64::from(x) + 1.0, f64::from(grid.height - png_y));
            let Some(hit) = sample_at(&input, ra, dec, false) else {
                continue;
            };
            if hit.band != Some(band) {
                continue;
            }
            // flux and variance were f32 planes; these casts are exact.
            accumulator.add(
                grid_pixel_rect(x, png_y, grid.width, grid.height)?,
                &Measurement {
                    band,
                    value: hit.flux as f32,
                    variance: hit.variance as f32,
                    flags: hit.flags,
                    flag_bits: cutout.flag_bits,
                },
            );
        }
    }
    Ok(())
}

/// Reads members one band at a time. Only 256×256 tiles touched by member
/// support are allocated, but within that support the RAM cost per touched grid
/// pixel while one sector is built is:
/// - `SectorBuilder`: 18 B (two f64 sums + a u16 band count), for the union of
///   tiles touched by any band of the sector, held until the sector finishes;
/// - the band in flight (`BandAccumulator`): 24 B (two f64 sums + a u64 count),
///   for the tiles that band touches;
/// - `SectorBuilder::finish`: 8 B (f32 value + f32 variance) for every tile with
///   a complete pixel, held until `FrameFile::create` has written it to disk.
///
/// So a sector covering the whole grid peaks near 18 + 24 = 42 B per pixel during
/// accumulation and 18 + 8 = 26 B per pixel at finish; for a fully covered
/// 30-degree grid (≈3.1e8 pixels) that is ≈13 GB. Builds are serialized (one at a
/// time) for this reason. After a frame is stored, only its tile offsets stay in
/// RAM.
pub struct SectorBuild {
    pub image: SectorImage,
    pub contributions: Vec<BandContribution>,
    pub count_bytes: u64,
    pub staging: artifact::Staging,
    pub bytes: u64,
    pub requests: u64,
}

pub enum BuildOutcome {
    Hit(Arc<Artifact>),
    Built(SectorBuild),
}

pub type BuildResult = Result<BuildOutcome, String>;

/// Fetch one member cutout. Tests substitute synthetic cutouts and a recorded
/// call order; production reads the exact S3 window.
pub async fn build_sector<F, Fut>(
    fetch: F,
    grid: &TanGrid,
    members: &[Arc<SnippetPlan>],
    staging: artifact::Staging,
) -> Result<SectorBuild, String>
where
    F: Fn(&Arc<SnippetPlan>) -> Fut,
    Fut: std::future::Future<Output = Result<Cutout, String>>,
{
    let counts_path = staging.path().join("counts.bin");
    match build_sector_inner(fetch, grid, members, &counts_path).await {
        Ok((image, contributions, count_bytes, bytes, requests)) => Ok(SectorBuild {
            image,
            contributions,
            count_bytes,
            staging,
            bytes,
            requests,
        }),
        Err(error) => {
            // Early sampling/build failure: the partial sidecar is not a count result.
            let _ = std::fs::remove_file(&counts_path);
            Err(error)
        }
    }
}

async fn build_sector_inner<F, Fut>(
    fetch: F,
    grid: &TanGrid,
    members: &[Arc<SnippetPlan>],
    counts_path: &Path,
) -> Result<(SectorImage, Vec<BandContribution>, u64, u64, u64), String>
where
    F: Fn(&Arc<SnippetPlan>) -> Fut,
    Fut: std::future::Future<Output = Result<Cutout, String>>,
{
    let mut by_band: BTreeMap<u16, Vec<&Arc<SnippetPlan>>> = BTreeMap::new();
    members
        .iter()
        .for_each(|plan| by_band.entry(plan.support.band).or_default().push(plan));
    // The same canonical order the fingerprint hashes; otherwise reversing the
    // submitted members could change floating-point addition order for one key.
    for plans in by_band.values_mut() {
        provenance::canonicalize(plans);
    }
    let mut builder =
        SectorBuilder::new_rect(by_band.keys().copied().collect(), grid.width, grid.height)?;
    let mut counts =
        CountWriter::create_rect(counts_path, builder.bands(), grid.width, grid.height)?;
    let (mut bytes, mut requests) = (0, 0);
    for (band, plans) in by_band {
        let mut accumulator = BandAccumulator::new_rect(band, grid.width, grid.height);
        for plan in plans {
            let cutout = fetch(plan).await?;
            bytes += cutout.bytes_read;
            requests += cutout.s3_requests;
            let grid = grid.clone();
            let bounds = plan.support.grid_bounds;
            accumulator = tokio::task::spawn_blocking(move || {
                sample_member(&cutout, &grid, &mut accumulator, Some(bounds)).map(|_| accumulator)
            })
            .await
            .map_err(|error| error.to_string())??;
        }
        counts.write_band(&accumulator)?;
        builder.add_band(&accumulator)?;
    }
    let counts = counts.finish()?;
    let image = builder.finish()?;
    Ok((image, counts.contributions, counts.bytes, bytes, requests))
}

/// Build one admitted sector. The cached artifact path is checked before any
/// source is constructed, so a rename/reorder/preview miss does not open S3.
pub async fn run(
    owner: Arc<Mutex<Build>>,
    grid: TanGrid,
    admitted: Admitted,
    jobs: Arc<crate::Jobs>,
) {
    let _permit = match jobs.builds.clone().acquire_owned().await {
        Ok(permit) => permit,
        Err(error) => {
            publish(&owner, |build| {
                set_state(
                    build,
                    "failed",
                    Some(format!("Sector build queue unavailable: {error}")),
                )
            });
            return;
        }
    };
    if !publish(&owner, |build| set_state(build, "building", None)) {
        return;
    }
    let Some(fingerprint) = snapshot(&owner, |build| build.info.sector.fingerprint.clone()) else {
        return;
    };
    if let Some(artifact) = jobs.cached_artifact(&fingerprint) {
        let Some(info) = snapshot(&owner, |build| build.info.sector.clone()) else {
            return;
        };
        finish(
            &owner,
            &jobs,
            store_result(
                info,
                Ok(BuildOutcome::Hit(artifact)),
                grid.width,
                grid.height,
            ),
        );
        return;
    }
    let staging = match artifact::Staging::create(&crate::sequence::sequence_root(), &fingerprint) {
        Ok(staging) => staging,
        Err(error) => {
            publish(&owner, |build| {
                set_state(
                    build,
                    "failed",
                    Some(format!(
                        "Sector '{}' artifact staging failed: {error}",
                        build.info.sector.id
                    )),
                )
            });
            return;
        }
    };
    let source = match SpherexSource::direct() {
        Ok(source) => source,
        Err(error) => {
            publish(&owner, |build| {
                set_state(
                    build,
                    "failed",
                    Some(format!(
                        "Sector '{}' source unavailable: {error}",
                        build.info.sector.id
                    )),
                )
            });
            return;
        }
    };
    let source = &source;
    let fetch = |plan: &Arc<SnippetPlan>| {
        let plan = plan.clone();
        async move {
            source
                .exact_window_cutout(&plan.file, &plan.support.window)
                .await
                .map_err(|error| format!("{}: {error}", plan.file.key))
        }
    };
    let result = build_sector(&fetch, &grid, &admitted.members, staging)
        .await
        .map(BuildOutcome::Built);
    let Some(info) = snapshot(&owner, |build| build.info.sector.clone()) else {
        return;
    };
    let stored = match tokio::task::spawn_blocking(move || {
        store_result(info, result, grid.width, grid.height)
    })
    .await
    {
        Ok(stored) => stored,
        Err(error) => {
            let Some(info) = snapshot(&owner, |build| build.info.sector.clone()) else {
                return;
            };
            Stored {
                info: failed_info(info, format!("Sector frame could not be stored: {error}")),
                artifact: None,
            }
        }
    };
    finish(&owner, &jobs, stored);
}

fn ready_info(
    mut info: SectorFrameInfo,
    artifact: &Artifact,
    bytes_read: u64,
    requests: u64,
) -> SectorFrameInfo {
    info.state = "ready".into();
    info.error = None;
    info.complete_pixels = artifact.complete_pixels;
    info.target_coverage = artifact.target_coverage;
    info.target_covered = artifact.target_coverage >= 0.2;
    info.data_tiles = artifact.data_tiles.clone();
    info.contributions = Some(artifact.contributions.clone());
    info.count_bytes = Some(artifact.count_bytes);
    info.bytes_read = bytes_read;
    info.requests = requests;
    info
}

fn failed_info(mut info: SectorFrameInfo, error: String) -> SectorFrameInfo {
    info.state = "failed".into();
    info.error = Some(error);
    info.complete_pixels = 0;
    info.target_coverage = 0.0;
    info.target_covered = false;
    info.data_tiles = Vec::new();
    info.contributions = None;
    info.count_bytes = None;
    info
}

pub struct Stored {
    pub info: SectorFrameInfo,
    pub artifact: Option<Arc<Artifact>>,
}

/// Record one finished sector. A build that produced no complete pixel is a
/// failed sector, never a playable blank frame. Its aggregate per-band summary
/// is still reported, but the count sidecar is not retained without a frame.
pub fn store_result(
    mut info: SectorFrameInfo,
    result: BuildResult,
    width: u32,
    height: u32,
) -> Stored {
    let mut artifact = None;
    match result {
        Ok(BuildOutcome::Hit(cached)) => {
            info = ready_info(info, &cached, 0, 0);
            artifact = Some(cached);
        }
        Ok(BuildOutcome::Built(build)) => {
            info.bytes_read = build.bytes;
            info.requests = build.requests;
            if build.image.complete_pixels == 0 {
                let error = format!(
                    "Sector '{}' has no pixel with every selected band; remove a band or a member before playback",
                    info.id
                );
                let contributions = build.contributions.clone();
                drop(build.staging);
                info.state = "failed".into();
                info.complete_pixels = 0;
                info.target_coverage = 0.0;
                info.target_covered = false;
                info.data_tiles = Vec::new();
                info.contributions = Some(contributions);
                info.count_bytes = None;
                info.error = Some(error);
            } else {
                match artifact::create(
                    build.staging,
                    info.fingerprint.clone(),
                    build.image,
                    build.contributions,
                    build.count_bytes,
                    width,
                    height,
                ) {
                    Ok(stored) => {
                        info = ready_info(info, &stored, build.bytes, build.requests);
                        artifact = Some(stored);
                    }
                    Err(error) => {
                        let id = info.id.clone();
                        info = failed_info(
                            info,
                            format!("Sector '{id}' frame could not be stored: {error}"),
                        );
                    }
                }
            }
        }
        Err(error) => {
            info = failed_info(info, error);
        }
    }
    Stored { info, artifact }
}

fn finish(owner: &Mutex<Build>, jobs: &Arc<crate::Jobs>, stored: Stored) {
    if let Some(artifact) = &stored.artifact {
        jobs.cache_artifact(artifact.clone());
    }
    publish(owner, |build| {
        build.info.sector = stored.info;
        build.info.state = build.info.sector.state.clone();
        build.info.error = build.info.sector.error.clone();
    });
}

fn set_state(build: &mut Build, state: &str, error: Option<String>) {
    build.info.state = state.into();
    build.info.error = error.clone();
    build.info.sector.state = state.into();
    build.info.sector.error = error;
}

/// Read under the lock, then release it. `None` once the build is released.
fn snapshot<T>(owner: &Mutex<Build>, read: impl FnOnce(&Build) -> T) -> Option<T> {
    let build = owner.lock().ok()?;
    (!build.released).then(|| read(&build))
}

/// Lock briefly to apply a finished result. Returns `false`, changing nothing,
/// once the build was released (deleted, expired or superseded).
fn publish(owner: &Mutex<Build>, change: impl FnOnce(&mut Build)) -> bool {
    let Ok(mut build) = owner.lock() else {
        return false;
    };
    if build.released {
        return false;
    }
    change(&mut build);
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::browse::Support;
    use crate::reduction::{grid_coordinates, DataTile, DataTiles};
    use crate::types::{SnippetRow, TILE_SIZE};
    use spearv_core::{
        FlagBits, PixelWindow, Plane, Poly, Sip, WavelengthMap, Wcs, PIXEL_SCALE_DEG,
    };
    use spearv_index::{ExposureRow, Quality, Survey};

    fn wcs() -> Wcs {
        Wcs {
            crpix: [3.0, 3.0],
            crval: [10.0, 20.0],
            cd: [[-PIXEL_SCALE_DEG, 0.0], [0.0, PIXEL_SCALE_DEG]],
            sip: Sip {
                a: Poly::new(0),
                b: Poly::new(0),
                ap: None,
                bp: None,
            },
            naxis: [5, 5],
        }
    }
    pub(crate) fn region() -> ObservationRegion {
        ObservationRegion {
            ra: 10.0,
            dec: 20.0,
            fov: 5.0 * PIXEL_SCALE_DEG,
            fov_l: None,
            fov_h: None,
        }
    }
    pub(crate) fn plan(
        id: &str,
        band: u16,
        mjd: f64,
        region: ObservationRegion,
    ) -> Arc<SnippetPlan> {
        let row = SnippetRow {
            id: id.into(),
            snippet_ref: id.into(),
            band: band + 1,
            mjd,
            utc: String::new(),
            release: "qr2".into(),
            detector: 1,
            obsid: id.into(),
            version: "v".into(),
            geometric_samples: 1,
            wl_min_um: 0.75,
            wl_max_um: 0.75,
            target_covered: true,
            quality: serde_json::Value::Null,
        };
        let file = ExposureRow {
            key: format!("key-{id}"),
            url: String::new(),
            etag: Some("\"e\"".into()),
            size: None,
            release: "qr2".into(),
            survey: Survey::AllSky,
            sps_flags: None,
            period: String::new(),
            version: "v".into(),
            obsid: id.into(),
            detector: 1,
            mjd_beg: None,
            mjd_avg: mjd,
            em_min_um: 0.7,
            em_max_um: 1.1,
            wcs: wcs(),
            quality: Quality::default(),
            sc_xyz: None,
        };
        let support = Support {
            grid_bounds: [0, 4, 0, 4],
            band,
            count: 1,
            window: PixelWindow {
                x0: 0,
                y0: 0,
                width: 5,
                height: 5,
                clipped: false,
            },
            wl_min: 0.75,
            wl_max: 0.75,
        };
        Arc::new(SnippetPlan {
            region,
            file,
            support,
            row,
        })
    }
    fn sector(id: &str, snippets: &[&str]) -> SequenceSector {
        SequenceSector {
            id: id.into(),
            revision: 1,
            snippets: snippets.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn lookup(plans: Vec<Arc<SnippetPlan>>) -> impl Fn(&str) -> Option<Arc<SnippetPlan>> {
        move |reference| plans.iter().find(|plan| plan.row.id == reference).cloned()
    }

    #[test]
    fn admission_keeps_order_and_refuses_shared_empty_unknown_or_foreign_snippets() {
        let other = ObservationRegion {
            fov: 1.0,
            ..region()
        };
        let plans = vec![
            plan("a", 0, 2.0, region()),
            plan("b", 1, 1.0, region()),
            plan("c", 0, 3.0, region()),
            plan("z", 0, 1.0, other),
        ];
        let order = admit(
            &region(),
            vec![sector("s3", &["c"]), sector("s1", &["a", "b"])],
            lookup(plans.clone()),
        )
        .unwrap();
        assert_eq!(
            order
                .iter()
                .map(|item| item.sector.id.as_str())
                .collect::<Vec<_>>(),
            ["s3", "s1"]
        );
        let info = frame_info(&order[1]).unwrap();
        assert_eq!(info.bands, vec![1, 2]);
        assert_eq!((info.mjd_start, info.mjd_end), (1.0, 2.0));
        assert!(!info.utc_start.is_empty() && !info.utc_end.is_empty());
        assert_eq!(info.releases, vec!["qr2"]);
        assert_eq!(
            info.members
                .iter()
                .map(|member| member.snippet.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"],
            "public member order is submitted order"
        );
        assert_eq!(
            info.members[0].s3_key, order[1].members[0].file.key,
            "exact S3 key is retained"
        );
        assert_eq!(
            info.members[0].etag, order[1].members[0].file.etag,
            "exact ETag is retained"
        );
        assert_eq!(
            info.fingerprint,
            provenance::fingerprint(&region(), &order[1].members)
        );
        for (sectors, expected) in [
            (
                vec![sector("s1", &["a"]), sector("s2", &["a"])],
                "exactly one sector",
            ),
            (vec![sector("s1", &["a", "a"])], "exactly one sector"),
            (vec![sector("s1", &[])], "is empty"),
            (vec![sector("s1", &["missing"])], "Unknown snippet"),
            (vec![sector("s1", &["z"])], "different region"),
            (vec![sector("s1", &["a"]), sector("s1", &["b"])], "repeated"),
            (vec![], "at least one"),
        ] {
            let refusal = admit(&region(), sectors, lookup(plans.clone()))
                .err()
                .unwrap();
            assert!(refusal.message().contains(expected), "{refusal:?}");
            assert_eq!(
                matches!(refusal, Refusal::UnknownSnippet(_)),
                expected == "Unknown snippet",
                "only an unknown ref is a not-found refusal: {refusal:?}"
            );
        }
    }

    #[test]
    fn sector_id_and_revision_do_not_change_the_render_fingerprint() {
        let plans = vec![plan("a", 0, 2.0, region()), plan("b", 1, 1.0, region())];
        let first = admit(
            &region(),
            vec![sector("Sector 1", &["a", "b"])],
            lookup(plans.clone()),
        )
        .unwrap();
        let mut renamed = admit(
            &region(),
            vec![sector("Renamed", &["a", "b"])],
            lookup(plans),
        )
        .unwrap();
        renamed[0].sector.revision = 9;
        let a = frame_info(&first[0]).unwrap();
        let b = frame_info(&renamed[0]).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.id, b.id);
        assert_ne!(a.revision, b.revision);
    }

    fn cutout(value: f32, variance: f32, wavelength: f32, flags: Vec<u32>) -> Cutout {
        let plane = |v| Plane {
            width: 5,
            height: 5,
            data: vec![v; 25],
        };
        Cutout {
            image: plane(value + 1.0),
            variance: plane(variance),
            zodi: Some(plane(1.0)),
            flags: Plane {
                width: 5,
                height: 5,
                data: flags,
            },
            flags_valid: None,
            wcs: wcs(),
            wavelength: WavelengthMap {
                x: vec![1.0, 5.0],
                y: vec![1.0, 5.0],
                wavelength: vec![wavelength; 4],
                bandwidth: vec![0.02; 4],
                nx: 2,
                ny: 2,
            },
            origin_fits: (1, 1),
            clipped: false,
            flag_bits: FlagBits {
                transient: 0,
                overflow: 1,
                sur_error: 2,
                nonfunc: 6,
                fullsample: 12,
                persist: 17,
                outlier: 19,
                source: 21,
            },
            bytes_read: 0,
            s3_requests: 0,
        }
    }

    fn at(image: &SectorImage, width: u32, pixel: usize) -> (f32, f32) {
        let (x, y) = grid_coordinates(pixel, width).unwrap();
        image
            .tiles
            .get(&(x / TILE_SIZE, y / TILE_SIZE))
            .map(|tile| {
                let index = DataTile::index(x, y);
                (tile.value[index], tile.variance[index])
            })
            .unwrap_or((f32::NAN, f32::NAN))
    }

    #[test]
    fn grid_sampling_feeds_the_approved_reducer_in_png_order() {
        let grid = region().grid();
        let pixels = (grid.width * grid.width) as usize;
        // Band 0 at 0.753 µm twice (depth), band 2 at 0.79 µm once (bandwise).
        let mut flags = vec![0; 25];
        flags[0] = 1 << 6; // FITS (1,1) is the bottom-left: PNG bottom row.
        let mut band0 = BandAccumulator::new(0, grid.width);
        sample_member(&cutout(2.0, 1.0, 0.753, flags), &grid, &mut band0, None).unwrap();
        sample_member(
            &cutout(10.0, 4.0, 0.753, vec![0; 25]),
            &grid,
            &mut band0,
            None,
        )
        .unwrap();
        let mut band2 = BandAccumulator::new(2, grid.width);
        sample_member(
            &cutout(12.0, 9.0, 0.79, vec![0; 25]),
            &grid,
            &mut band2,
            None,
        )
        .unwrap();
        let mut wrong = BandAccumulator::new(2, grid.width);
        sample_member(
            &cutout(12.0, 9.0, 0.753, vec![0; 25]),
            &grid,
            &mut wrong,
            None,
        )
        .unwrap();
        assert!(
            (0..pixels).all(|pixel| wrong.depth(pixel).samples == 0),
            "other band never contributes"
        );
        let mut builder = SectorBuilder::new(vec![0, 2], grid.width).unwrap();
        builder.add_band(&band0).unwrap();
        builder.add_band(&band2).unwrap();
        let image = builder.finish().unwrap();
        assert_eq!(image.complete_pixels, pixels as u64);
        let bottom_left = ((grid.width - 1) * grid.width) as usize;
        // Masked first exposure leaves only 10.0 in band 0 there: (10 + 12) / 2.
        assert!((at(&image, grid.width, bottom_left).0 - 11.0).abs() < 1e-5);
        // Elsewhere: band 0 = (2/1 + 10/4)/(1 + 1/4) = 3.6, sector = (3.6 + 12)/2.
        assert!((at(&image, grid.width, 0).0 - 7.8).abs() < 1e-5);
        assert!((at(&image, grid.width, 0).1 - 2.45).abs() < 1e-5);
    }

    #[test]
    fn a_small_footprint_allocates_only_its_tile_on_a_large_grid() {
        let cutout = cutout(2.0, 1.0, 0.753, vec![0; 25]);
        let grid = TanGrid::new(10.0, 20.0, 300.0 * PIXEL_SCALE_DEG);
        let geometry = spearv_core::ExposureGeometry {
            wcs: cutout.wcs.clone(),
            wavelength: cutout.wavelength.clone(),
            header_bytes: 0,
        };
        let support = crate::browse::supports(&geometry, &grid, &[0]).remove(0);
        let mut band = BandAccumulator::new(0, grid.width);
        sample_member(&cutout, &grid, &mut band, Some(support.grid_bounds)).unwrap();
        assert_eq!(band.tiles().len(), 1, "the 5x5 support is one data tile");
        let mut builder = SectorBuilder::new(vec![0], grid.width).unwrap();
        builder.add_band(&band).unwrap();
        let image = builder.finish().unwrap();
        assert_eq!(
            image.tiles.len(),
            1,
            "a 300x300 grid has four tiles if dense"
        );
        assert_eq!(image.data_tiles(), vec![[0, 0]]);
        assert_eq!(
            image.tiles[&(0, 0)].value.len(),
            (TILE_SIZE * TILE_SIZE) as usize
        );
        assert!(!image.tiles.contains_key(&(1, 1)));
    }

    #[test]
    fn exact_discovery_bounds_match_full_grid_sampling_at_every_pixel() {
        let mut cutout = cutout(2.0, 1.0, 0.753, vec![0; 25]);
        cutout.wavelength.wavelength = vec![0.753, 0.753, 0.79, 0.79];
        let grid = TanGrid::new(10.0, 20.0, 9.0 * PIXEL_SCALE_DEG);
        let geometry = spearv_core::ExposureGeometry {
            wcs: cutout.wcs.clone(),
            wavelength: cutout.wavelength.clone(),
            header_bytes: 0,
        };
        let pixels = (grid.width * grid.width) as usize;
        let found = crate::browse::supports(&geometry, &grid, &[0, 2]);
        assert_eq!(found.len(), 2);
        for support in found {
            let mut full = BandAccumulator::new(support.band, grid.width);
            let mut bounded = BandAccumulator::new(support.band, grid.width);
            sample_member(&cutout, &grid, &mut full, None).unwrap();
            sample_member(&cutout, &grid, &mut bounded, Some(support.grid_bounds)).unwrap();
            let [left, right, bottom, top] = support.grid_bounds;
            assert!((right - left + 1) * (top - bottom + 1) < grid.width * grid.width);
            for pixel in 0..pixels {
                let expected = full.depth(pixel);
                let actual = bounded.depth(pixel);
                assert_eq!(actual.samples, expected.samples);
                assert_eq!(actual.value, expected.value);
                assert_eq!(actual.variance, expected.variance);
            }
        }
    }

    fn temp_root() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "spearv-sector-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    async fn ordered_build(
        region: &ObservationRegion,
        values: &BTreeMap<String, f32>,
        members: &[Arc<SnippetPlan>],
        root: &Path,
        name: &str,
    ) -> (SectorBuild, std::path::PathBuf, Vec<String>) {
        let values = values.clone();
        let order = Arc::new(Mutex::new(Vec::new()));
        let order_for_fetch = order.clone();
        let fetch = move |plan: &Arc<SnippetPlan>| {
            let id = plan.row.id.clone();
            let value = values[&id];
            let order = order_for_fetch.clone();
            async move {
                order.lock().unwrap().push(id);
                Ok(cutout(value, 1.0, 0.753, vec![0; 25]))
            }
        };
        let staging = artifact::Staging::create(root, name).unwrap();
        let grid = region.grid();
        let built = build_sector(&fetch, &grid, members, staging).await.unwrap();
        let staging = built.staging.path().to_path_buf();
        let order = order.lock().unwrap().clone();
        (built, staging, order)
    }

    #[tokio::test]
    async fn canonical_execution_makes_reversed_members_byte_identical_and_same_fingerprint() {
        let region = region();
        let values = [("a", 1e16_f32), ("b", -1e16_f32), ("c", 1.0_f32)];
        let plans: Vec<_> = values
            .iter()
            .map(|(id, _)| plan(id, 0, 1.0, region.clone()))
            .collect();
        let find = lookup(plans.clone());
        let admitted_order = admit(&region, vec![sector("s1", &["a", "b", "c"])], find).unwrap();
        let find = lookup(plans.clone());
        let admitted_reverse = admit(&region, vec![sector("s1", &["c", "b", "a"])], find).unwrap();
        let forward_info = frame_info(&admitted_order[0]).unwrap();
        let reverse_info = frame_info(&admitted_reverse[0]).unwrap();
        assert_eq!(
            forward_info.fingerprint, reverse_info.fingerprint,
            "submitted order must not change the fingerprint"
        );
        assert_eq!(
            forward_info
                .members
                .iter()
                .map(|member| member.snippet.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"],
            "public member order stays submitted"
        );

        let root = temp_root();
        let values: BTreeMap<String, f32> = values
            .iter()
            .map(|(id, value)| ((*id).to_string(), *value))
            .collect();
        let (forward, forward_staging, forward_order) = ordered_build(
            &region,
            &values,
            &admitted_order[0].members,
            &root,
            "forward",
        )
        .await;
        let (reverse, reverse_staging, reverse_order) = ordered_build(
            &region,
            &values,
            &admitted_reverse[0].members,
            &root,
            "reverse",
        )
        .await;
        assert_eq!(
            forward_order, reverse_order,
            "members are fetched in the same canonical order"
        );
        let first_path = forward_staging.join("frame.bin");
        crate::sequence::FrameFile::create(&first_path, forward.image.tiles).unwrap();
        let first_bytes = std::fs::read(&first_path).unwrap();
        let second_path = reverse_staging.join("frame.bin");
        crate::sequence::FrameFile::create(&second_path, reverse.image.tiles).unwrap();
        let second_bytes = std::fs::read(&second_path).unwrap();
        assert_eq!(first_bytes, second_bytes, "frame bytes are byte-identical");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_failed_band_removes_the_partial_count_sidecar() {
        let region = region();
        let plans = vec![
            plan("a", 0, 1.0, region.clone()),
            plan("b", 1, 2.0, region.clone()),
        ];
        let admitted = admit(&region, vec![sector("s1", &["a", "b"])], lookup(plans)).unwrap();
        let root = temp_root();
        let staging = artifact::Staging::create(&root, "partial").unwrap();
        let counts_path = staging.path().join("counts.bin");
        let path = counts_path.clone();
        let observed = Arc::new(Mutex::new(false));
        let observed_in_fetch = observed.clone();
        let fetch = move |plan: &Arc<SnippetPlan>| {
            let path = path.clone();
            let observed = observed_in_fetch.clone();
            let fail = plan.support.band == 1;
            let wavelength = if fail { 0.771 } else { 0.753 };
            async move {
                if fail {
                    *observed.lock().unwrap() = path.exists();
                    return Err("synthetic sampling failure".to_string());
                }
                Ok(cutout(2.0, 1.0, wavelength, vec![0; 25]))
            }
        };
        let grid = region.grid();
        let result = build_sector(&fetch, &grid, &admitted[0].members, staging).await;
        assert!(result.is_err());
        assert!(
            *observed.lock().unwrap(),
            "the first band had already written a partial sidecar"
        );
        assert!(
            !counts_path.exists(),
            "the partial count sidecar is removed on early failure"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn aborting_an_artifact_build_removes_its_staging_directory() {
        let root = temp_root();
        let staging = artifact::Staging::create(&root, "abort").unwrap();
        let directory = staging.path().to_path_buf();
        let region = region();
        let plans = vec![plan("a", 0, 1.0, region.clone())];
        let task = tokio::spawn(async move {
            let fetch = |_plan: &Arc<SnippetPlan>| async {
                std::future::pending::<Result<Cutout, String>>().await
            };
            build_sector(&fetch, &region.grid(), &plans, staging).await
        });
        tokio::task::yield_now().await;
        task.abort();
        let error = match task.await {
            Ok(_) => panic!("the aborted build must not complete"),
            Err(error) => error,
        };
        assert!(error.is_cancelled());
        assert!(
            !directory.exists(),
            "an aborted build leaves no partial artifact directory"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn sector_build(
        root: &Path,
        fingerprint: &str,
        image: SectorImage,
        bands: &[u16],
    ) -> SectorBuild {
        let staging = artifact::Staging::create(root, fingerprint).unwrap();
        let counts_path = staging.path().join("counts.bin");
        let mut writer = CountWriter::create(&counts_path, bands, 2).unwrap();
        for band in bands {
            writer.write_band(&BandAccumulator::new(*band, 2)).unwrap();
        }
        let counts = writer.finish().unwrap();
        SectorBuild {
            image,
            contributions: counts.contributions,
            count_bytes: counts.bytes,
            staging,
            bytes: 12,
            requests: 3,
        }
    }

    fn artifact_for(
        root: &Path,
        fingerprint: &str,
        tiles: DataTiles,
        complete_pixels: u64,
    ) -> Arc<Artifact> {
        let staging = artifact::Staging::create(root, fingerprint).unwrap();
        std::fs::write(staging.path().join("counts.bin"), b"counts").unwrap();
        artifact::create(
            staging,
            fingerprint.into(),
            SectorImage {
                tiles,
                complete_pixels,
            },
            vec![],
            6,
            2,
            2,
        )
        .unwrap()
    }

    fn build_info(sector: SectorFrameInfo) -> SectorBuildInfo {
        SectorBuildInfo {
            build_id: "build".into(),
            width: 5,
            height: 5,
            tile_size: TILE_SIZE,
            state: "building".into(),
            error: None,
            sector,
            reduction: REDUCTION.into(),
        }
    }

    #[test]
    fn a_zero_complete_build_result_is_recorded_as_failed() {
        let holder = frame_info(
            &admit(
                &region(),
                vec![sector("solo", &["a"])],
                lookup(vec![plan("a", 0, 1.0, region())]),
            )
            .unwrap()[0],
        )
        .unwrap();
        let root = temp_root();
        let build = sector_build(
            &root,
            "solo-fingerprint",
            SectorImage {
                tiles: DataTiles::new(),
                complete_pixels: 0,
            },
            &[0],
        );
        let staging = build.staging.path().to_path_buf();
        let stored = store_result(holder, Ok(BuildOutcome::Built(build)), 2, 2);
        assert!(stored.artifact.is_none(), "zero-complete is never cached");
        let info = stored.info;
        assert_eq!(info.state, "failed");
        assert_eq!(info.bytes_read, 12);
        assert!(info.contributions.is_some(), "all bands were accumulated");
        assert!(
            info.count_bytes.is_none(),
            "no sidecar is retained without a frame"
        );
        assert!(!staging.exists());
        let error = info.error.unwrap();
        assert!(error.contains("solo"), "{error}");
        assert!(
            error.contains("no pixel with every selected band"),
            "{error}"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn zero_complete_builds_are_never_stored_as_a_disk_frame() {
        let holder = frame_info(
            &admit(
                &region(),
                vec![sector("solo", &["a"])],
                lookup(vec![plan("a", 0, 1.0, region())]),
            )
            .unwrap()[0],
        )
        .unwrap();
        let root = temp_root();
        let build = sector_build(
            &root,
            "zero-fingerprint",
            SectorImage {
                tiles: DataTiles::new(),
                complete_pixels: 0,
            },
            &[0],
        );
        let stored = store_result(holder, Ok(BuildOutcome::Built(build)), 2, 2);
        assert!(stored.artifact.is_none());
        assert_eq!(stored.info.state, "failed");
        assert!(!root.join("frame.bin").exists());
        assert!(
            std::fs::read_dir(&root).unwrap().next().is_none(),
            "the failed build's staging directory is removed"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_cache_hit_reports_zero_source_reads_and_keeps_the_artifact_until_idle() {
        let jobs = Arc::new(crate::Jobs::default());
        let region = region();
        let plans = vec![plan("a", 0, 1.0, region.clone())];
        let admitted = admit(&region, vec![sector("s1", &["a"])], lookup(plans))
            .unwrap()
            .remove(0);
        let sector_info = frame_info(&admitted).unwrap();
        let fingerprint = sector_info.fingerprint.clone();
        let root = temp_root();
        let tiles = DataTiles::from([(
            (0, 0),
            DataTile {
                value: vec![1.0; (TILE_SIZE * TILE_SIZE) as usize],
                variance: vec![0.25; (TILE_SIZE * TILE_SIZE) as usize],
            },
        )]);
        let artifact = artifact_for(&root, &fingerprint, tiles, 2);
        jobs.cache_artifact(artifact);
        let owner = Arc::new(Mutex::new(Build::new(build_info(sector_info))));
        run(owner.clone(), region.grid(), admitted, jobs.clone()).await;
        {
            let build = owner.lock().unwrap();
            assert_eq!(build.info.state, "ready");
            assert_eq!(build.info.sector.state, "ready");
            assert_eq!(build.info.sector.bytes_read, 0);
            assert_eq!(build.info.sector.requests, 0);
            assert_eq!(build.info.sector.complete_pixels, 2);
            assert!(build.info.sector.contributions.is_some());
            assert_eq!(build.info.sector.count_bytes, Some(6));
        }
        assert!(jobs.cached_artifact(&fingerprint).is_some());
        jobs.expire(std::time::Instant::now() + crate::resources::IDLE);
        assert!(jobs.cached_artifact(&fingerprint).is_none());
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "idle eviction removes the unreferenced artifact directory"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
