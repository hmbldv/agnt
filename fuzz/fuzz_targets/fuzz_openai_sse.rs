#![no_main]
// Fuzz target: feed arbitrary bytes through the OpenAI SSE parser.
//
// Added in v0.3.1 after the adversarial review flagged that
// `agnt-net/src/backend.rs` is the largest source file in the runtime
// (814 LOC) and had zero fuzz coverage. A broken or hostile proxy can
// emit malformed `data:` lines, missing field separators, giant blobs,
// or non-UTF-8 bytes — the parser must neither panic nor crash.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = agnt_net::backend::_fuzz_parse_openai_stream(data);
});
