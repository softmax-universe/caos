// Build a full-buffer snapshot for the pinned native anchor resolver. The
// layouts are test-only; production resolves anchors in its own replay state.
const assert = require('node:assert/strict');

function anchorReference(e, memory) {
  const {alloc, bytes, view, u32, decode, check} = memory;
  const buffer = decode('buffer_decode', '92009201c40100', 256).output;
  const integer = value => {
    assert.ok(value >= 0 && value < 65536);
    if (value < 128) return value.toString(16).padStart(2, '0');
    if (value < 256) return `cc${value.toString(16).padStart(2, '0')}`;
    return `cd${value.toString(16).padStart(4, '0')}`;
  };
  return (text, timestamp, actor, offset, right) => {
    const snapshot = alloc(272);
    bytes().set(bytes().slice(buffer, buffer + 72), snapshot);
    e.text_clone(snapshot + 72, text);
    const string = alloc(12);
    e.text_string(string, text);
    u32(snapshot + 264, 0);
    u32(snapshot + 268, view().getUint32(string + 8, true));
    const actorID = Buffer.alloc(16);
    actorID[0] = actor;
    const wire = `92029392009201c40100929292${integer(timestamp)}9201c410` +
      `${actorID.toString('hex')}c2${integer(offset)}${right ? '01' : '00'}`;
    const anchor = decode('anchor_decode', wire, 256);
    check('native anchor consumes input', anchor.remaining, 0);
    const result = alloc(8);
    e.anchor_resolve(result, snapshot, anchor.output);
    const present = view().getUint32(result, true);
    assert.ok(present === 0 || present === 1);
    return present ? view().getUint32(result + 4, true) : null;
  };
}

module.exports = {anchorReference};
