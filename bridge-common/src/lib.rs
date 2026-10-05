//! What every codec family of the bridge shares, and nothing that decodes:
//! the state a codec path reads besides its own ([`shared`]), the host log
//! sink ([`logging`]), PCM and frame helpers ([`frame_builders`]) and the
//! sparse object↔channel declaration ([`objects`]).
//!
//! No decoder crate may be a dependency here: a family crate depends on this
//! one and on its own decoders only (`scripts/check-crate-isolation.sh`).

pub mod frame_builders;
pub mod logging;
pub mod objects;
pub mod shared;
