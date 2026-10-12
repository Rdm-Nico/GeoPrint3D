//! GeoPrint3D backend: the terrain/building generation pipeline as a library,
//! shared by the HTTP server (`main.rs`) and the city regression harness
//! (`bin/city_tests`, see `tests/cities/README.md`).

pub mod api;
pub mod models;
pub mod services;
pub mod utils;
