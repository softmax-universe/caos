// Pure native codec checks for every metadata type the one-shot writer emits.
function verifyMessageWriteReference(e, memory) {
  const {decode, encode, check} = memory;
  const actor = '9201c410000102030405060708090a0b0c0d0e0f';
  const buffer = '92009201c40100';
  const insertion = `920b${actor}`;
  const anchor = offset => `920293${buffer}9292${insertion}c2${offset.toString(16).padStart(2, '0')}01`;
  const start = anchor(2), end = anchor(7);
  const user = `92009891${actor}91b543414f532066697874757265206f70657261746f7290c29192${start}${end}9090917b`;
  const assistant = `92019b91${actor}9090917b90909090917b9090`;
  const metadata = '9391ae6578706c696369742d6d6f64656c9192a9616e7468726f706963ae6578706c696369742d6d6f64656c90';
  for (const [name, wire, decoder, encoder] of [
    ['tagged UUID actor', actor, 'actor_decode', 'actor_encode'],
    ['user message and exact anchors', user, 'message_decode', 'message_encode'],
    ['completed assistant turn', assistant, 'message_decode', 'message_encode'],
    ['actual agent provider and model', metadata, 'actor_metadata_decode', 'actor_metadata_encode'],
  ]) {
    const value = decode(decoder, wire, 4096);
    check(`${name} consumes input`, value.remaining, 0);
    // Result<ActorIdStub, Error> has a one-byte success discriminant before
    // the value in this build. The other structs use an error niche in-place.
    const resultOffset = decoder === 'actor_decode' ? 1 : 0;
    check(`${name} native roundtrip`, encode(encoder, value.output + resultOffset), wire);
  }
}

module.exports = {verifyMessageWriteReference};
