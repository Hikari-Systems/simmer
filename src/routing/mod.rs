//! §3.2 route selection: match a sender rule (§5.4), resolve the recipient's
//! domain group (step 2), order the chain for thread affinity (step 2a, D-090),
//! then walk it reserving quota (step 3).

pub mod chain;
pub mod domain_group;
pub mod sender_match;
pub mod thread;
