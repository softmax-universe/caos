// Pure in-memory text operations from the pinned public client. This is
// compatibility test equipment, never an execution dependency of the runner.
const assert = require('node:assert/strict');

function verifyTextReference(e, memory) {
  const {alloc, bytes, view, u32, u64, encode, decode, check} = memory;
  const empty = () => {
    const text = alloc(192); // Pinned Text<DeltaId, ArcAllocator>.
    e.text_empty(text);
    return text;
  };
  const clone = text => {
    const copy = alloc(192);
    e.text_clone(copy, text);
    return copy;
  };
  const string = text => {
    const output = alloc(12);
    e.text_string(output, text);
    const pointer = view().getUint32(output + 4, true);
    const length = view().getUint32(output + 8, true);
    return new TextDecoder('utf8', {fatal: true}).decode(bytes().slice(pointer, pointer + length));
  };
  const id = (timestamp, actor) => {
    const pointer = alloc(32);
    u64(pointer, BigInt(timestamp));
    bytes()[pointer + 8] = 1; // ActorId::Uuid.
    bytes()[pointer + 9] = actor;
    return pointer;
  };
  const apply = (text, timestamp, actor, wire, parent = 0) => {
    const plan = decode('edit_decode', wire, 64);
    check('text edit consumes its input', plan.remaining, 0);
    assert.notEqual(view().getUint32(plan.output, true), 0, 'native edit decode error');
    check('text edit canonical encoding', encode('edit_encode', plan.output), wire);
    const result = alloc(128);
    e.text_apply(result, text, id(timestamp, actor), plan.output, parent);
    check('native text operation succeeds', bytes()[result + 8], 2);
    return string(text);
  };
  const integer = value => {
    assert.ok(value >= 0 && value < 65536);
    if (value < 128) return value.toString(16).padStart(2, '0');
    if (value < 256) return `cc${value.toString(16).padStart(2, '0')}`;
    return `cd${value.toString(16).padStart(4, '0')}`;
  };
  const edit = (text, splices) => {
    const data = Buffer.from(text);
    const count = 1 + 3 * splices.length;
    assert.ok(count < 16 && data.length < 32);
    return (0x90 + count).toString(16) + (0xa0 + data.length).toString(16) +
      data.toString('hex') + splices.flat().map(integer).join('');
  };
  const initial = empty();
  check('empty native text', string(initial), '');
  check('insert native text', apply(initial, 1, 1, edit('abc', [[0, 0, 3]])), 'abc');
  check('insert at a byte offset', apply(initial, 2, 1, edit('X', [[1, 0, 1]])), 'aXbc');
  const unicode = empty();
  apply(unicode, 1, 1, edit('😀é', [[0, 0, 6]]));
  check('native offsets count UTF-8 bytes',
    apply(unicode, 2, 1, edit('X', [[4, 0, 1]])), '😀Xé');
  const base = empty(), left = clone(base), right = clone(base);
  const x = edit('X', [[0, 0, 1]]), y = edit('Y', [[0, 0, 1]]);
  apply(left, 1, 1, x, base);
  apply(right, 1, 2, y, base);
  check('concurrent actor ordering', apply(left, 1, 2, y, base), 'YX');
  check('concurrent delivery converges', apply(right, 1, 1, x, base), 'YX');
  const creation = decode('scratch_decode', '91a3616263', 64);
  const created = alloc(192);
  e.text_create(created, id(1, 1), creation.output);
  check('scratch creation text', string(created), 'abc');
  for (const isBase of [false, true]) {
    const fragment = decode('fragment_decode',
      `929292019201c41001000000000000000000000000000000${isBase ? 'c3' : 'c2'}00`, 64);
    const result = alloc(8);
    e.fragment_offset(result, created, fragment.output, 0);
    check('scratch creation uses an ordinary insertion ID', bytes()[result + 4], isBase ? 2 : 1);
  }
  const batch = alloc(96), payload = alloc(32);
  u64(batch, 42n);
  bytes()[batch + 8] = 1;
  u32(batch + 80, 3);
  for (let index = 0; index < 3; index++) {
    e.batch_payload_id(payload, batch, index);
    check('payload IDs end at the batch clock',
      view().getBigUint64(payload, true), BigInt(40 + index));
  }

  // Generate portable cases with forks and multi-parent merges. Every edit is
  // interpreted in its exact parent snapshot, not in an actor-clock prefix.
  let seed = 73;
  const random = limit => {
    seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0;
    return seed % limit;
  };
  const cases = [];
  for (let caseIndex = 0; caseIndex < 128; caseIndex++) {
    const steps = [], ancestors = [], bases = [], cache = new Map();
    const materialize = selected => {
      const key = [...selected].sort((a, b) => a - b).join(',');
      if (cache.has(key)) return cache.get(key);
      const text = empty();
      for (let index = 0; index < steps.length; index++) {
        if (!selected.has(index)) continue;
        const step = steps[index];
        apply(text, step.timestamp, step.actor, step.wire, bases[index]);
      }
      cache.set(key, text);
      return text;
    };
    for (let index = 0; index < 9; index++) {
      const parents = index === 0 ? [] : [random(index)];
      if (index > 2 && random(3) === 0) {
        const other = random(index);
        if (!parents.includes(other)) parents.push(other);
      }
      const selected = new Set();
      for (const parent of parents) {
        for (const ancestor of ancestors[parent]) selected.add(ancestor);
        selected.add(parent);
      }
      const basis = materialize(selected), points = [0];
      let length = 0;
      for (const character of string(basis)) {
        length += Buffer.byteLength(character);
        points.push(length);
      }
      let start = random(points.length), end = random(points.length);
      if (start > end) [start, end] = [end, start];
      const chunks = ['', 'X', 'é', '😀', 'ab'];
      const inserted = chunks[random(chunks.length)];
      let text = inserted;
      const splices = [[points[start], points[end] - points[start], Buffer.byteLength(inserted)]];
      if (random(3) === 0) {
        const a = end + random(points.length - end);
        const b = a + random(points.length - a);
        const second = chunks[random(chunks.length)];
        splices.push([points[a], points[b] - points[a], Buffer.byteLength(second)]);
        text += second;
      }
      bases.push(basis);
      ancestors.push(selected);
      steps.push({timestamp: index + 1, actor: 1 + random(2), parents, wire: edit(text, splices)});
    }
    const all = new Set(steps.map((_, index) => index));
    const expected = string(materialize(all));
    const parents = new Set(steps.flatMap(step => step.parents));
    const heads = steps.map((_, index) => index).filter(index => !parents.has(index));
    cases.push({name: `synthetic-${caseIndex}`, steps, heads, expected});
  }
  return cases;
}

module.exports = {verifyTextReference};
