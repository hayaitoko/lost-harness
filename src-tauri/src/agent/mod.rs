//! Agent core — gate, loop, and tool dispatch.
//!
//! This module is the M1 surface. M0 has it as a stub.
//! For M1 we ship:
//!   - `gate`   — §7 PrivacyGate + Binding + GateDecision
//!   - `egress` — is_private_endpoint() for the network-egress control
//!
//! The agent loop and tool registry come in later milestones.

pub mod egress;
pub mod gate;

#[cfg(test)]
mod gate_tests;
