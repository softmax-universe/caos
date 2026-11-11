// Check observed-remove map semantics using only the pinned public client's
// in-memory snapshots. Snapshot/entry offsets are build-specific test inputs.
const assert = require('node:assert/strict');

function verifyValueReference(e, memory) {
  const {alloc, bytes, view, u64, decode, check} = memory;
  const empty = () => {
    const snapshot = alloc(64);
    e.snapshot_empty(snapshot);
    return snapshot;
  };
  const clone = snapshot => {
    const copy = alloc(64);
    e.snapshot_clone(copy, snapshot);
    return copy;
  };
  const id = (timestamp, actor) => {
    const pointer = alloc(32);
    u64(pointer, BigInt(timestamp));
    bytes()[pointer + 8] = 1;
    bytes()[pointer + 9] = actor;
    return pointer;
  };
  const first = decode('map_insert_decode', '939201c40101c4016bc40141', 128).output;
  const second = decode('map_insert_decode', '939201c40101c4016bc40142', 128).output;
  const remove = decode('map_remove_decode', '929201c40101c4016b', 128).output;
  const winner = snapshot => {
    // Maps are at snapshot +52; a MapInsertDelta starts with its MapId, then
    // key Bytes at +32. MapValue starts with its winning DeltaId.
    const map = e.map_get(snapshot + 52, first);
    if (!map) return null;
    const values = e.map_values(map, first + 32);
    if (!values) return null;
    const value = e.map_winner(values);
    assert.notEqual(value, 0);
    return {timestamp: view().getBigUint64(value, true), actor: bytes()[value + 9]};
  };
  const base = empty(), left = clone(base), right = clone(base);
  e.map_insert(left, id(2, 1), first, base);
  e.map_insert(right, id(3, 2), second, base);
  const merged = clone(left);
  e.map_insert(merged, id(3, 2), second, base);
  check('concurrent map winner timestamp', winner(merged).timestamp, 3n);
  check('concurrent map winner actor', winner(merged).actor, 2);
  e.map_remove(merged, remove, right);
  check('removing the observed winner reveals an unseen value', winner(merged).actor, 1);
  check('unseen map value keeps its original identity', winner(merged).timestamp, 2n);
  const untouched = clone(left);
  e.map_remove(untouched, remove, base);
  check('a removal cannot erase an unseen insertion', winner(untouched).actor, 1);
  const cleared = clone(left);
  e.map_insert(cleared, id(3, 2), second, base);
  const both = clone(cleared);
  e.map_remove(cleared, remove, both);
  check('removing all observed concurrent values empties the key', winner(cleared), null);
}

module.exports = {verifyValueReference};
