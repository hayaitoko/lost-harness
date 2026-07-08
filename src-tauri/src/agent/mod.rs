//! Agent core — gate, loop, and tool dispatch.
//!
//! This module is the M1 surface. M0 has it as a stub.
//! For M1 we ship:
//!   - `gate`     — §7 PrivacyGate + Binding + GateDecision
//!   - `egress`   — is_private_endpoint() for the network-egress control
//!   - `loop_mod` — §9 AgentLoop (gate + model + storage, streams tokens)
//!
//! The tool registry comes in later milestones.

pub mod egress;
pub mod gate;
pub mod loop_mod;

#[cfg(test)]
mod gate_tests;
#[cfg(test)]
mod loop_tests;
