#![no_main]
// Fuzz target: feed arbitrary bytes through the Anthropic SSE parser.
//
// Anthropic's event stream has multiple event kinds (content_block_start,
// content_block_delta, content_block_stop, message_stop, …) with JSON
// payloads. Mutating the field shapes should never panic the parser.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = agnt_net::backend::_fuzz_parse_anthropic_stream(data);
});
