// Read schema descriptors produced by the pinned reference module. These are
// build-specific Rust memory layouts, never a production wire-format decoder.
const assert = require('node:assert/strict');

function recoverSchemas(e) {
  const bytes = () => new Uint8Array(e.memory.buffer);
  const view = () => new DataView(e.memory.buffer);
  const u32 = p => view().getUint32(p, true);
  const alloc = size => {
    const p = e.malloc(size);
    assert.notEqual(p, 0);
    bytes().fill(0, p, p + size);
    return p;
  };
  const string = p => {
    const capacity = u32(p), pointer = u32(p + 4), length = u32(p + 8);
    assert.ok(capacity < 0x80000000 && length <= capacity && length < 65536);
    assert.ok(pointer + length <= e.memory.buffer.byteLength);
    return new TextDecoder('utf8', {fatal: true}).decode(bytes().subarray(pointer, pointer + length));
  };
  const schema = (p, depth = 0) => {
    assert.ok(depth < 64, 'schema descriptor nesting');
    const tag = u32(p);
    if (tag < 0x80000000) return {kind: 19, named: string(p)};
    const kind = tag - 0x80000000;
    assert.ok(kind <= 23);
    if ([16, 17, 21, 22, 23].includes(kind)) {
      return {kind, element: schema(u32(p + 4), depth + 1)};
    }
    if (kind === 18) {
      return {kind, key: schema(u32(p + 4), depth + 1), value: schema(u32(p + 8), depth + 1)};
    }
    return kind === 20 ? {kind, parameter: u32(p + 4)} : {kind};
  };
  // Rust reorders these node layouts differently for each instantiation.
  // Offsets were checked against the compiled keys/values/length accessors.
  const layouts = {
    definitions: {size: 712, length: 710, keys: 4, keySize: 12, values: 136, valueSize: 52},
    fields: {size: 328, length: 314, keys: 316, keySize: 1, values: 4, valueSize: 28},
    variants: {size: 284, length: 270, keys: 272, keySize: 1, values: 0, valueSize: 24},
    set: {size: 20, length: 6, keys: 8, keySize: 1, values: 0, valueSize: 0},
  };
  const tree = (p, layoutName) => {
    const layout = layouts[layoutName];
    const root = u32(p), height = u32(p + 4), count = u32(p + 8), entries = [];
    assert.ok(count < 10000);
    if (!root) {
      assert.equal(count, 0);
      return entries; // The height in an empty Rust BTreeMap is uninitialized.
    }
    assert.ok(height < 8);
    const walk = (node, level) => {
      if (!node) return;
      assert.ok(node + layout.size <= e.memory.buffer.byteLength);
      const length = view().getUint16(node + layout.length, true);
      assert.ok(length <= 11);
      for (let i = 0; i < length; i++) {
        if (level) walk(u32(node + layout.size + i * 4), level - 1);
        entries.push([node + layout.keys + i * layout.keySize,
          node + layout.values + i * layout.valueSize]);
      }
      if (level) walk(u32(node + layout.size + length * 4), level - 1);
    };
    walk(root, height);
    assert.equal(entries.length, count, 'B-tree entry count');
    return entries;
  };
  const collector = alloc(128), result = alloc(128), emptyGroup = alloc(32);
  // The collector contains a visited-name HashSet plus a definition BTreeMap.
  // Use valid empty control bytes and fixed hash seeds: no random/browser calls.
  bytes().fill(255, emptyGroup, emptyGroup + 32);
  view().setUint32(collector, emptyGroup, true);
  e.schema_thread(result, collector);
  const threadType = string(result);
  e.schema_batch(result, collector);
  const batchType = string(result);
  const definitions = {};
  for (const [key, p] of tree(collector + 32, 'definitions')) {
    const name = string(key), kind = bytes()[p + 48], definition = {kind};
    if (kind === 3) {
      definition.alias = u32(p) === 0xFFFFFFFF ? null : schema(p);
      definition.fields = {};
      for (const [field, value] of tree(p + 12, 'fields')) {
        const flag = bytes()[value + 24];
        assert.ok(flag <= 1);
        definition.fields[bytes()[field]] = {
          name: u32(value) === 0xFFFFFFFF ? null : string(value),
          schema: schema(value + 12), flag,
        };
      }
      definition.retired = tree(p + 24, 'set').map(([field]) => bytes()[field]);
    } else if (kind === 2) {
      definition.alias = schema(p);
    } else {
      assert.ok(kind === 0 || kind === 1);
      definition.variants = {};
      for (const [tag, value] of tree(p, 'variants')) {
        definition.variants[bytes()[tag]] = u32(value) === 0xFFFFFFFE
          ? {name: string(value + 4), unit: true}
          : {name: string(value), schema: schema(value + 12)};
      }
      definition.tag_sets = [12, 24, 36].map(offset =>
        tree(p + offset, 'set').map(([tag]) => bytes()[tag]));
    }
    assert.ok(!Object.hasOwn(definitions, name));
    definitions[name] = definition;
  }
  assert.equal(Object.keys(definitions).length, 553);
  const checkReferences = value => {
    if (!value || typeof value !== 'object') return;
    if (value.named) assert.ok(Object.hasOwn(definitions, value.named), value.named);
    for (const child of Object.values(value)) checkReferences(child);
  };
  for (const definition of Object.values(definitions)) {
    // A struct's extra declaration name is metadata, not a reference to a
    // registered specialization (e.g. Register versus Register<String>).
    checkReferences(definition.kind === 3 ? definition.fields : definition);
  }
  return {roots: [threadType, batchType], definitions};
}

module.exports = {recoverSchemas};
