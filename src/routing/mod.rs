//! §3.2 route selection: match a sender rule (§5.4), resolve the recipient's
//! domain group (step 2), then walk the chain reserving quota (step 3).

pub mod chain;
pub mod domain_group;
pub mod sender_match;
