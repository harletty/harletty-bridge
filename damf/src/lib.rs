// Copyright 2025 Rainbaby
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Offline master-format writers: the Dolby Atmos Master File (DAMF) metadata
//! set plus the CAF and WAV containers its audio lives in.
//!
//! This crate is deliberately consumer-agnostic and I/O-shaped only: it knows
//! how to *write* the formats and nothing about how a bitstream is decoded. It
//! is used by the offline `truehdd` CLI and must never be reachable from the
//! realtime `harletty-bridge` plugin — see the crate-graph invariant in
//! docs/plan-truehdd-resurrection-in-harletty.md, which CI enforces.

// Vendored little/big-endian write helpers. The LE half is unused here — CAF
// is big-endian throughout and WAV writes its own headers — but `impl_u32_enum!`
// generates both halves for every CAF enum, so the trait has to exist.
#[allow(dead_code)]
mod byteorder;
pub mod caf;
mod damf;
pub mod wav;

pub use damf::*;
