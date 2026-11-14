//! Offline only: which CDN files cover a field, and how to read their rows.
//! The running server does not use this module.

use std::collections::BTreeSet;
use std::io::{self, Write};

use cdshealpix::nested;

/// File names are ranges of HEALPix depth 8 nested cells (nside 256).
pub const FILE_DEPTH: u8 = 8;
/// `source_id / 2^35` is the depth 12 cell. Four depths down is 2^8 more.
const CELL_SHIFT: u32 = 35 + 8;
/// A source_id cell comes from the position Gaia first assigned, which can sit
/// just across a cell edge from the published `ra` and `dec`.
pub const CELL_MARGIN_DEG: f64 = 0.02;

#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub name: &'static str,
    pub ra: f64,
    pub dec: f64,
}

pub const FIELDS: [Field; 3] = [
    Field { name: "pleiades", ra: 56.75, dec: 24.12 },
    Field { name: "geometria", ra: 58.2783, dec: 26.1398 },
    Field { name: "deep", ra: 270.146, dec: 66.523 },
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CdnFile {
    pub name: String,
    pub first_cell: u32,
    pub last_cell: u32,
    pub md5: String,
}

/// Read `_MD5SUM.txt`. Lines for anything but a data file are ignored.
pub fn parse_manifest(text: &str) -> Vec<CdnFile> {
    text.lines().filter_map(parse_manifest_line).collect()
}

fn parse_manifest_line(line: &str) -> Option<CdnFile> {
    let (md5, name) = line.split_once(char::is_whitespace)?;
    let name = name.trim();
    let range = name.strip_prefix("GaiaSource_")?.strip_suffix(".csv.gz")?;
    let (first, last) = range.split_once('-')?;
    Some(CdnFile {
        name: name.to_string(),
        first_cell: first.parse().ok()?,
        last_cell: last.parse().ok()?,
        md5: md5.to_string(),
    })
}

pub fn cell_of_source(source_id: i64) -> u32 {
    (source_id >> CELL_SHIFT) as u32
}

/// Every depth 8 cell that touches a cone.
pub fn cone_cells(ra: f64, dec: f64, radius_deg: f64) -> BTreeSet<u32> {
    nested::get(FILE_DEPTH)
        .cone_coverage_approx(ra.to_radians(), dec.to_radians(), radius_deg.to_radians())
        .flat_iter()
        .map(|cell| cell as u32)
        .collect()
}

pub fn cell_of_sky(ra: f64, dec: f64) -> u32 {
    nested::get(FILE_DEPTH).hash(ra.to_radians(), dec.to_radians()) as u32
}

/// The files whose cell range holds any of `cells`.
pub fn files_for_cells<'a>(files: &'a [CdnFile], cells: &BTreeSet<u32>) -> Vec<&'a CdnFile> {
    files
        .iter()
        .filter(|file| cells.range(file.first_cell..=file.last_cell).next().is_some())
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Star {
    pub source_id: i64,
    pub ra: f64,
    pub dec: f64,
    pub pmra: Option<f64>,
    pub pmdec: Option<f64>,
    pub g: Option<f64>,
    pub ref_epoch: f64,
}

/// Column positions, taken from the header line rather than assumed.
#[derive(Clone, Copy, Debug)]
pub struct Columns {
    source_id: usize,
    ra: usize,
    dec: usize,
    pmra: usize,
    pmdec: usize,
    g: usize,
    ref_epoch: usize,
    last: usize,
}

impl Columns {
    pub fn from_header(line: &str) -> Result<Self, String> {
        let names: Vec<&str> = line.trim_end().split(',').collect();
        let find = |name: &str| {
            names
                .iter()
                .position(|column| *column == name)
                .ok_or_else(|| format!("column {name} is missing from the header"))
        };
        let (source_id, ra, dec) = (find("source_id")?, find("ra")?, find("dec")?);
        let (pmra, pmdec) = (find("pmra")?, find("pmdec")?);
        let (g, ref_epoch) = (find("phot_g_mean_mag")?, find("ref_epoch")?);
        let last = [source_id, ra, dec, pmra, pmdec, g, ref_epoch].into_iter().max().unwrap_or(0);
        Ok(Self { source_id, ra, dec, pmra, pmdec, g, ref_epoch, last })
    }

    /// The source_id alone, so most rows are dropped without parsing the rest.
    pub fn source_id(&self, line: &str) -> Option<i64> {
        line.split(',').nth(self.source_id)?.parse().ok()
    }

    /// No column up to `last` holds a comma. Only `designation` is quoted there.
    pub fn star(&self, line: &str) -> Option<Star> {
        self.star_filtered(line, None).ok().flatten()
    }

    /// Parse one CSV row, returning `Ok(None)` when `max_mag` is set and the row
    /// has a finite `phot_g_mean_mag` fainter than `max_mag`. Rows with missing
    /// `phot_g_mean_mag` are retained.
    pub fn star_filtered(&self, line: &str, max_mag: Option<f64>) -> Result<Option<Star>, &'static str> {
        let mut source_id = None;
        let mut ra = None;
        let mut dec = None;
        let mut pmra = None;
        let mut pmdec = None;
        let mut g = None;
        let mut ref_epoch = None;

        for (index, cell) in line.split(',').enumerate() {
            if index == self.source_id {
                source_id = Some(cell);
            } else if index == self.ra {
                ra = Some(cell);
            } else if index == self.dec {
                dec = Some(cell);
            } else if index == self.pmra {
                pmra = Some(cell);
            } else if index == self.pmdec {
                pmdec = Some(cell);
            } else if index == self.g {
                g = Some(cell);
            } else if index == self.ref_epoch {
                ref_epoch = Some(cell);
            }
            if index == self.last {
                break;
            }
        }

        let g = optional(g.ok_or("missing column")?);
        if let (Some(limit), Some(mag)) = (max_mag, g) {
            if mag > limit {
                return Ok(None);
            }
        }

        let ra: f64 = ra.ok_or("missing column")?.parse().map_err(|_| "invalid ra")?;
        let dec: f64 = dec.ok_or("missing column")?.parse().map_err(|_| "invalid dec")?;
        let ref_epoch: f64 = ref_epoch.ok_or("missing column")?.parse().map_err(|_| "invalid ref_epoch")?;
        if !ra.is_finite() || !dec.is_finite() || !ref_epoch.is_finite() {
            return Err("non-finite astrometry");
        }

        Ok(Some(Star {
            source_id: source_id.ok_or("missing column")?.parse().map_err(|_| "invalid source_id")?,
            ra,
            dec,
            pmra: optional(pmra.ok_or("missing column")?),
            pmdec: optional(pmdec.ok_or("missing column")?),
            g,
            ref_epoch,
        }))
    }
}

/// Gaia writes a missing value as the word `null`.
fn optional(cell: &str) -> Option<f64> {
    cell.parse().ok().filter(|value: &f64| value.is_finite())
}

/// Calls `on_line` for each complete line of a byte stream, without the newline.
/// Sits behind the gzip decoder, so a 230 MB file never has to be held.
pub struct LineSplitter<F> {
    partial: Vec<u8>,
    on_line: F,
}

impl<F: FnMut(&str) -> io::Result<()>> LineSplitter<F> {
    pub fn new(on_line: F) -> Self {
        Self { partial: Vec::new(), on_line }
    }

    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        let line = std::str::from_utf8(bytes).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        (self.on_line)(line)
    }
}

impl<F: FnMut(&str) -> io::Result<()>> Write for LineSplitter<F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut rest = buf;
        while let Some(end) = rest.iter().position(|byte| *byte == b'\n') {
            if self.partial.is_empty() {
                self.emit(&rest[..end])?;
            } else {
                self.partial.extend_from_slice(&rest[..end]);
                let line = std::mem::take(&mut self.partial);
                self.emit(&line)?;
            }
            rest = &rest[end + 1..];
        }
        self.partial.extend_from_slice(rest);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::{GzEncoder, MultiGzDecoder};
    use flate2::Compression;

    const HEADER: &str = "solution_id,designation,source_id,ref_epoch,ra,dec,pmra,pmdec,phot_g_mean_mag,other";

    #[test]
    fn manifest_keeps_data_files_and_drops_itself() {
        let files = parse_manifest(
            "52d8  GaiaSource_000000-003111.csv.gz\nc084  GaiaSource_786097-786431.csv.gz\n2b91  _MD5SUM.txt\n",
        );
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].first_cell, 0);
        assert_eq!(files[0].last_cell, 3111);
        assert_eq!(files[1].last_cell, 786431);
        assert_eq!(files[1].md5, "c084");
    }

    #[test]
    fn source_id_gives_the_depth_8_cell() {
        // The first row of the first CDN file sits in the first cell.
        assert_eq!(cell_of_source(4295806720), 0);
        // Barnard's star, whose sky position gives the same cell.
        assert_eq!(cell_of_source(4472832130942575872), cell_of_sky(269.4486, 4.7394));
    }

    #[test]
    fn the_last_cell_of_the_sky_is_inside_the_last_file() {
        assert_eq!(cell_of_source((786431_i64 << 43) + 5), 786431);
    }

    #[test]
    fn a_cone_contains_the_cell_of_its_centre() {
        for field in FIELDS {
            let cells = cone_cells(field.ra, field.dec, 0.5);
            assert!(cells.contains(&cell_of_sky(field.ra, field.dec)), "{}", field.name);
            // A 0.5 degree cone is about 15 cells of 0.052 square degrees, plus its rim.
            assert!((10..80).contains(&cells.len()), "{} {}", field.name, cells.len());
        }
    }

    #[test]
    fn files_are_chosen_by_cell_range() {
        let files = parse_manifest("a GaiaSource_000000-000009.csv.gz\nb GaiaSource_000010-000019.csv.gz\nc GaiaSource_000020-000029.csv.gz\n");
        let picked = files_for_cells(&files, &BTreeSet::from([9, 21]));
        assert_eq!(picked.iter().map(|file| file.md5.as_str()).collect::<Vec<_>>(), ["a", "c"]);
    }

    #[test]
    fn rows_are_read_by_header_name_and_null_stays_missing() {
        let columns = Columns::from_header(HEADER).unwrap();
        let full = columns.star(r#"163,"Gaia DR3 4295806720",4295806720,2016.0,44.99,0.0056,11.9,-4.08,17.64,x"#).unwrap();
        assert_eq!(full.source_id, 4295806720);
        assert_eq!(full.pmra, Some(11.9));
        assert_eq!(full.g, Some(17.64));
        let bare = columns.star(r#"163,"Gaia DR3 2512",2512,2016.0,45.07,0.11,null,null,null,x"#).unwrap();
        assert_eq!((bare.pmra, bare.pmdec, bare.g), (None, None, None));
        assert_eq!(columns.source_id("1,\"d\",77,2016.0"), Some(77));
    }

    #[test]
    fn star_filtered_keeps_bright_and_null_and_drops_faint() {
        let columns = Columns::from_header(HEADER).unwrap();
        let bright = r#"163,"Gaia DR3 1",1,2016.0,45.0,0.0,1.0,-1.0,16.5,x"#;
        let faint = r#"163,"Gaia DR3 2",2,2016.0,45.0,0.0,1.0,-1.0,17.01,x"#;
        let null_g = r#"163,"Gaia DR3 3",3,2016.0,45.0,0.0,null,null,null,x"#;
        assert_eq!(columns.star_filtered(bright, Some(17.0)).unwrap().unwrap().source_id, 1);
        assert!(columns.star_filtered(faint, Some(17.0)).unwrap().is_none());
        assert_eq!(columns.star_filtered(null_g, Some(17.0)).unwrap().unwrap().source_id, 3);
        assert!(columns.star_filtered("163,short", Some(17.0)).is_err());
    }

    #[test]
    fn a_header_without_a_needed_column_is_an_error() {
        assert!(Columns::from_header("source_id,ra,dec").is_err());
    }

    #[test]
    fn gzip_chunks_become_whole_lines_at_any_split() {
        let text = "# comment\nfirst,line\nsecond,line\n";
        let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(text.as_bytes()).unwrap();
        let gz = encoder.finish().unwrap();
        for size in [1, 3, 7, gz.len()] {
            let mut lines = Vec::new();
            let mut decoder = MultiGzDecoder::new(LineSplitter::new(|line: &str| {
                lines.push(line.to_string());
                Ok(())
            }));
            gz.chunks(size).for_each(|chunk| decoder.write_all(chunk).unwrap());
            decoder.finish().unwrap();
            assert_eq!(lines, ["# comment", "first,line", "second,line"], "chunk size {size}");
        }
    }
}
