//! Docker orchestration via bollard.
//!
//! Handles: container listing, deployment (build + run),
//! log streaming, restart/scale operations, and Docker Swarm
//! service orchestration.

pub mod client;

pub use client::{ClusterNode, DockerClient};
