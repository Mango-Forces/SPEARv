Hey! Please do not exclude the work from global judging due to this commit. 

I have not edited the project itself in any way, shape or form since the hackathon deadline, however, I passed out and could not write the credits in time. 

Unlike other projects presented at our local event (Tbilisi, Technopark) our project was actually fully functional, while most teams delivered only an advert for what they "will" do.

We worked very hard on making SPEARv functional and comfortable over the two days of the challenge, we hope that it'll be beneficial to you (NASA), or to anybody else. 

You can check it out at mango.forces.quest it'll load up SPEARv with some default setting. Althrough we intent to polish this project up as a portfolio piece, the served project at this URL will be the project finished during the competition. The main branch will have the competition work, while for future stuff we will make another branch. 


Well. These are the credits requested - 

### Data

| Resource | Provider | URL |
| --- | --- | --- |
| SPHEREx Level-2 FITS (public S3) | NASA IRSA | https://nasa-irsa-spherex.s3.us-east-1.amazonaws.com |
| SPHEREx archive | NASA IRSA | https://irsa.ipac.caltech.edu/data/SPHEREx/ |
| AllWISE catalog (parquet) | NASA IRSA / WISE | https://nasa-irsa-wise.s3.us-west-2.amazonaws.com/wise/allwise/catalogs/p3as_psd/healpix_k5/wise-allwise.parquet |
| WISE / AllWISE mission page | NASA IRSA | https://irsa.ipac.caltech.edu/Missions/wise.html |
| Gaia DR3 source catalog | ESA Gaia | https://cdn.gea.esac.esa.int/Gaia/gdr3/gaia_source/ |
| Gaia Archive | ESA | https://gea.esac.esa.int/archive/ |


### Engine (Rust)

| Library | Role | URL |
| --- | --- | --- |
| [Rust](https://www.rust-lang.org/) | Engine, index, FITS path, extensions | https://www.rust-lang.org/ |
| [Axum](https://github.com/tokio-rs/axum) | HTTP API (engine + index-service) | https://github.com/tokio-rs/axum |
| [Tokio](https://tokio.rs/) | Async runtime | https://tokio.rs/ |
| [Utoipa](https://github.com/juhaku/utoipa) | OpenAPI from Rust routes | https://github.com/juhaku/utoipa |
| [utoipa-axum](https://github.com/juhaku/utoipa) | Axum integration for Utoipa | https://github.com/juhaku/utoipa |
| [Serde](https://serde.rs/) / serde_json | JSON request/response types | https://serde.rs/ |
| [thiserror](https://github.com/dtolnay/thiserror) | Typed errors | https://github.com/dtolnay/thiserror |
| [async-trait](https://github.com/dtolnay/async-trait) | Async traits for sources/extensions | https://github.com/dtolnay/async-trait |
| [Reqwest](https://github.com/seanmonstar/reqwest) (rustls) | HTTP client (S3, index, catalogs) | https://github.com/seanmonstar/reqwest |
| [futures](https://github.com/rust-lang/futures-rs) | Async streams / combinators | https://github.com/rust-lang/futures-rs |
| [Rusqlite](https://github.com/rusqlite/rusqlite) (bundled) | Exposure index DB + Gaia SQLite | https://github.com/rusqlite/rusqlite |
| [CFITSIO](https://heasarc.gsfc.nasa.gov/fitsio/) 4.7.0 (HEASARC) | FITS I/O via private static build (no CFITSIO network) | https://heasarc.gsfc.nasa.gov/fitsio/ |
| [cc](https://github.com/rust-lang/cc-rs) | Build C shim linked to CFITSIO | https://github.com/rust-lang/cc-rs |
| [image](https://github.com/image-rs/image) + [png](https://github.com/image-rs/image-png) | Server PNG encode | https://github.com/image-rs/image |
| [PyO3](https://pyo3.rs/) | Bridge into pinned Python science runtime | https://pyo3.rs/ |
| [ndarray](https://github.com/rust-ndarray/ndarray) | Array types on the Rust science path | https://github.com/rust-ndarray/ndarray |
| [numpy](https://github.com/PyO3/rust-numpy) (Rust crate) | NumPy interop via PyO3 | https://github.com/PyO3/rust-numpy |
| [parquet](https://github.com/apache/arrow-rs) + [arrow-array](https://github.com/apache/arrow-rs) | AllWISE parquet reads | https://github.com/apache/arrow-rs |
| [cdshealpix](https://crates.io/crates/cdshealpix) | HEALPix indexing (Gaia build, WISE cones) | https://crates.io/crates/cdshealpix |
| [flate2](https://github.com/rust-lang/flate2-rs) | Gzip for Gaia bulk CSV.gz | https://github.com/rust-lang/flate2-rs |
| [chrono](https://github.com/chronotope/chrono) | Timestamps | https://github.com/chronotope/chrono |
| [sha2](https://github.com/RustCrypto/hashes) / [md-5](https://github.com/RustCrypto/hashes) / [hex](https://github.com/KokaKiwi/hex-rust) | Hashes and cache keys | https://github.com/RustCrypto/hashes |
| [bytes](https://github.com/tokio-rs/bytes) | Byte buffers | https://github.com/tokio-rs/bytes |
| [tower](https://github.com/tower-rs/tower) | HTTP utilities / tests | https://github.com/tower-rs/tower |
| [tempfile](https://github.com/Stebalien/tempfile) | Temporary files for render paths | https://github.com/Stebalien/tempfile |
| [tokio-util](https://github.com/tokio-rs/tokio) | Async IO helpers | https://github.com/tokio-rs/tokio |

### Web UI

| Library | Role | URL |
| --- | --- | --- |
| [React](https://react.dev/) | UI | https://react.dev/ |
| [React DOM](https://react.dev/) | DOM renderer | https://react.dev/ |
| [TypeScript](https://www.typescriptlang.org/) | Typed frontend | https://www.typescriptlang.org/ |
| [Vite](https://vite.dev/) | Dev server and production build | https://vite.dev/ |
| [@vitejs/plugin-react](https://github.com/vitejs/vite-plugin-react) | React plugin for Vite | https://github.com/vitejs/vite-plugin-react |
| [openapi-fetch](https://github.com/openapi-ts/openapi-typescript) | Typed HTTP client from OpenAPI | https://github.com/openapi-ts/openapi-typescript |
| [openapi-typescript](https://github.com/openapi-ts/openapi-typescript) | Generate TS types from OpenAPI | https://github.com/openapi-ts/openapi-typescript |

### Host and edge

| Tool | Role | URL |
| --- | --- | --- |
| OCI | Compute host | https://www.oracle.com/cloud/ |
| [Caddy](https://caddyserver.com/) | Reverse proxy and static site | https://caddyserver.com/ |
| [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/connections/connect-apps/) | Public HTTPS front door | https://developers.cloudflare.com/cloudflare-one/connections/connect-apps/ |
| systemd | Process units for engine, index, web | https://systemd.io/ |

### Method / challenge context

| Resource | URL |
| --- | --- |
| NASA Space Apps Challenge | https://www.spaceappschallenge.org/ |
| Backyard Worlds (citizen motion search reference) | https://www.backyardworlds.org/ |

---

