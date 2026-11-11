// Check independently composed write envelopes against the public native
// encoder. These synthetic batches never leave this offline process.
function verifyHistoryWriteReference(e, memory) {
  const {alloc, bytes, u32, copyHex, decode, encode, check} = memory;
  const uuid = '000102030405060708090a0b0c0d0e0f';
  const actor = `9201c410${uuid}`;
  const previous = `92cd0100${actor}`, next = `92cd0101${actor}`;
  const timeline = `9201${actor}`;
  const request = alloc(304);
  copyHex(request + 56, uuid);
  u32(request + 256, 7); // Pinned ClientMessage Rust discriminant: UpdateHistory.
  u32(request + 288, 42);
  check('no-op history update envelope', encode('client_encode', request),
    `922a920195c410${uuid}90909090`);

  // Batch Vec items are 88 bytes, timeline-update Vec items are 72 bytes.
  // TimelineOrigin and HistoryTimelineUpdate encode the same pair of fields.
  const batch = `94${next}c091${previous}91920590`;
  const update = `92${timeline}91${next}`;
  const decodedBatch = decode('batch_decode', batch, 256);
  const decodedUpdate = decode('origin_decode', update, 256);
  check('outgoing batch consumes its input', decodedBatch.remaining, 0);
  check('outgoing timeline update consumes its input', decodedUpdate.remaining, 0);
  check('outgoing batch roundtrip', encode('batch_encode', decodedBatch.output), batch);
  check('outgoing timeline update roundtrip', encode('origin_encode', decodedUpdate.output), update);
  u32(request + 20, 1);
  u32(request + 24, decodedBatch.output);
  u32(request + 28, 1);
  u32(request + 32, 1);
  u32(request + 36, decodedUpdate.output);
  u32(request + 40, 1);
  check('causal history update envelope', encode('client_encode', request),
    `922a920195c410${uuid}91${batch}91${update}9090`);
  const accepted = decode('server_decode', '9205922a92019100', 512);
  check('history update response accepted by native decoder',
    bytes()[accepted.output] === 255, false);
  check('history update response consumes input', accepted.remaining, 0);
}

module.exports = {verifyHistoryWriteReference};
