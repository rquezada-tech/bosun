//! Docker orchestration via bollard.
//!
//! Handles: container listing, deployment (build + run),
//! log streaming, restart/scale operations.

pub mod client;

pub use client::DockerClient;
