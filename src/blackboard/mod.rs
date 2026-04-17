//! Public blackboard surface.
//!
//! This module is a top-level alias over the blackboard types living in
//! [`crate::execute::blackboard`] plus the [`helpers`] submodule of small,
//! opinionated routines that compose cleanly over a [`Blackboard`].
//!
//! The helpers encode patterns originally developed in ergon-core's workflow
//! engine: tracking workflow inputs/results/constants, a stack of loop
//! variables for nested iteration, promoted values for name-addressable
//! downstream lookup, output-directory tracking, and a break signal for
//! loop-control nodes. They were upstreamed (SSOT §6.4.2) so other embedders
//! can reuse them without re-implementing the conventions.

pub use crate::execute::blackboard::{
    Blackboard, BlackboardScope, BlackboardSnapshot, ContextInheritance,
};

pub mod helpers;
