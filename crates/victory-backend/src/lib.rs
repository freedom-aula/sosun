//! The seam between victory-gpu and a host GPU API: traits only, no platform code.
//! `victory-metal` implements them on Metal; victory-gpu's native host is generic over them.

pub mod backend;
pub mod resources;
