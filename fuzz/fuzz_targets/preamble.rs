// A .tepin file an agent downloads is attacker-controlled input.
// The preamble parser must reject any garbage without panicking.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = tepin_core::format::parse_preamble(data);
});
