//! The stored companion policy bound, shared by every platform (the reservation itself is
//! Unix-only; the policy file is read on Windows too).

// Policy preparation carries at most the existing 17MiB companion envelope, plus one
// accepted roster token. Profile paths/names may need six-byte JSON escapes; policy
// retirement is bounded to64 entries by CompanionPolicy::start. Header space includes
// the repeated intent/identity, proof pair, phase/possession and JSON field framing.
pub const MAX_COMPANION_POLICY_JSON_BYTES: usize =
    xshell_protocol::ring::companion::MAX_COMPANION_FRAME
        .checked_add(xshell_protocol::ring::roster::MAX_ROSTER_TOKEN)
        .expect("policy token bound")
        .checked_add(
            6usize
                .checked_mul(
                    4096usize
                        .checked_mul(2)
                        .expect("profile stores")
                        .checked_add(1024)
                        .expect("profile endpoint")
                        .checked_add(128)
                        .expect("identity names"),
                )
                .expect("profile escapes"),
        )
        .expect("profile JSON bound")
        .checked_add(
            64usize
                .checked_mul(43 + 20 + 8)
                .expect("retired operations"),
        )
        .expect("retired JSON bound")
        .checked_add(8192)
        .expect("policy metadata framing");
