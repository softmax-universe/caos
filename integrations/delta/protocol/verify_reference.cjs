#!/usr/bin/env node
// Offline compatibility probe, not a runner. No Delta account or application.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const {recoverSchemas} = require('./schema_reference.cjs');
const {verifyTextReference} = require('./text_reference.cjs');
const {verifyValueReference} = require('./value_reference.cjs');
const {anchorReference} = require('./anchor_reference.cjs');
const {verifyHistoryWriteReference} = require('./history_write_reference.cjs');

const directory = process.argv[2];
const outputs = {};
let valid = !!directory && (process.argv.length - 3) % 2 === 0;
for (let index = 3; valid && index < process.argv.length; index += 2) {
  const flag = process.argv[index];
  valid = ['--schema-output', '--text-output'].includes(flag) && !Object.hasOwn(outputs, flag);
  if (valid) outputs[flag] = process.argv[index + 1];
}
if (!valid) {
  console.error('usage: node verify_reference.cjs <prepared-reference-directory> [--schema-output <file>] [--text-output <file>]');
  process.exit(2);
}

// Independently composed MessagePack fixtures, not bytes copied from the codec.
const integers = [
  [0n, '00'], [127n, '7f'], [128n, 'cc80'], [255n, 'ccff'],
  [256n, 'cd0100'], [65535n, 'cdffff'], [65536n, 'ce00010000'],
  [4294967295n, 'ceffffffff'], [4294967296n, 'cf0000000100000000'],
  [18446744073709551615n, 'cfffffffffffffffff'],
];
const uuid = '000102030405060708090a0b0c0d0e0f';
const oid = `${uuid}10111213`;
const runnerName = 'CAOS – eshear';
const runnerNameHex = 'af43414f5320e2809320657368656172';

async function main() {
  const metadata = JSON.parse(fs.readFileSync(path.join(directory, 'reference.json')));
  const module = await WebAssembly.compile(
    fs.readFileSync(path.join(directory, 'reference.wasm')),
  );
  assert.equal(metadata.build, '45918daa2b408d0f2efa573d44165d038c0d3109');
  assert.deepEqual(
    WebAssembly.Module.exports(module).map(x => x.name).sort(),
    ['memory', 'init_memory', 'malloc', 'git_header_encode', 'git_header_decode',
      'start_turn_encode', 'set_local_encode', 'ack_local_encode', 'client_encode',
      'server_decode', 'history_path', 'thread_encode', 'thread_decode', 'field_push',
      'batch_decode', 'batch_encode', 'origin_decode', 'origin_encode',
      'text_empty', 'text_string', 'text_clone', 'text_apply', 'edit_decode',
      'edit_encode', 'batch_payload_id',
      'text_create', 'scratch_decode', 'fragment_decode', 'fragment_offset',
      'buffer_decode', 'anchor_decode', 'anchor_resolve', 'anchor_resolve_visible',
      'snapshot_empty', 'snapshot_clone', 'map_insert', 'map_remove', 'map_get',
      'map_values', 'map_winner', 'map_insert_decode', 'map_remove_decode',
      'schema_thread', 'schema_batch'].sort(),
  );
  const imports = {};
  const hostCalls = [];
  for (const item of metadata.imports) {
    imports[item.module] ??= {};
    imports[item.module][item.name] = item.memory
      ? new WebAssembly.Memory(item.memory)
      : () => {
        hostCalls.push(item.name);
        throw new Error(`Browser/network/storage import forbidden: ${item.name}`);
      };
  }
  const instance = await WebAssembly.instantiate(module, imports);
  const e = instance.exports;
  e.init_memory(); // Passive static data initialization only; never call main.
  const bytes = () => new Uint8Array(e.memory.buffer);
  const view = () => new DataView(e.memory.buffer);
  const alloc = size => {
    const pointer = e.malloc(size);
    assert.notEqual(pointer, 0);
    bytes().fill(0, pointer, pointer + size);
    return pointer;
  };
  const u32 = (pointer, value) => view().setUint32(pointer, value, true);
  const u64 = (pointer, value) => view().setBigUint64(pointer, value, true);
  const copyHex = (pointer, value) => bytes().set(Buffer.from(value, 'hex'), pointer);
  const string = (pointer, value) => {
    if (value === null) {
      u32(pointer, 0xFFFFFFFF); // This pinned build's Option<String> niche.
      return;
    }
    const encoded = Buffer.from(value, 'utf8');
    const data = alloc(Math.max(encoded.length, 1));
    bytes().set(encoded, data);
    u32(pointer, encoded.length);
    u32(pointer + 4, data);
    u32(pointer + 8, encoded.length);
  };
  const encode = (name, pointer) => {
    const vector = alloc(12); // Pinned Rust Vec<u8>: capacity, pointer, length.
    u32(vector + 4, 1);
    assert.equal(e[name](pointer, vector), 0, `${name} returned an error`);
    const data = view().getUint32(vector + 4, true);
    const length = view().getUint32(vector + 8, true);
    assert.ok(length <= view().getUint32(vector, true));
    return Buffer.from(bytes().slice(data, data + length)).toString('hex');
  };
  const decode = (name, hex, size, fieldPath = null) => {
    const data = Buffer.from(hex, 'hex');
    const input = alloc(data.length);
    bytes().set(data, input);
    const slice = alloc(8), output = alloc(size), fieldId = alloc(24);
    u32(slice, input);
    u32(slice + 4, data.length);
    if (fieldPath !== null) {
      assert.ok(fieldPath.length <= 15);
      // Pinned Option<FieldId>: Some flag, then a 16-byte inline FieldId.
      bytes()[fieldId] = 1;
      bytes()[fieldId + 1] = fieldPath.length;
      bytes().set(fieldPath, fieldId + 2);
    }
    e[name](output, slice, fieldId);
    return {output, remaining: view().getUint32(slice + 4, true)};
  };
  const readHex = (pointer, length) =>
    Buffer.from(bytes().slice(pointer, pointer + length)).toString('hex');
  let checks = 0;
  const check = (name, actual, expected) => {
    assert.equal(actual, expected, name);
    checks++;
  };

  const historyPointer = alloc(16), pathOutput = alloc(12);
  copyHex(historyPointer, uuid);
  e.history_path(pathOutput, historyPointer);
  const pathPointer = view().getUint32(pathOutput + 4, true);
  const pathLength = view().getUint32(pathOutput + 8, true);
  check('per-history WebSocket path',
    Buffer.from(bytes().slice(pathPointer, pathPointer + pathLength)).toString(),
    '/history/00010203-0405-0607-0809-0a0b0c0d0e0f/connect');
  const historyID = `92cd01009201c410${uuid}`;
  const since = alloc(304);
  u64(since, 256n);
  bytes()[since + 8] = 1;
  copyHex(since + 9, uuid);
  copyHex(since + 32, uuid);
  u32(since + 256, 9); // FetchHistorySince's Rust discriminant.
  u32(since + 288, 42);
  check('history-index request envelope', encode('client_encode', since),
    `922a920693c410${uuid}${historyID}90`);
  const fetch = alloc(304);
  copyHex(fetch + 16, uuid);
  u32(fetch + 256, 10); // FetchHistoryDeltas's Rust discriminant.
  u32(fetch + 288, 42);
  check('history-deltas request envelope', encode('client_encode', fetch),
    `922a921b92c410${uuid}91c400`);
  const batch = `94${historyID}c09091920c911c`;
  for (const response of [
    `92009191${batch}`, `9202930191${batch}90`,
    `9202940191${batch}90a6667574757265`,
  ]) {
    const result = decode('server_decode', `9205922a921b${response}`, 512);
    check('history delta response accepted', bytes()[result.output] === 255, false);
    check('history delta response consumes input', result.remaining, 0);
  }
  for (const response of ['920293cf00000001000000009090', '92029301c090']) {
    const result = decode('server_decode', `9205922a921b${response}`, 512);
    check('malformed history response rejected', bytes()[result.output], 255);
  }

  for (const [epoch, encoded] of integers) {
    const turn = alloc(104);
    bytes()[turn + 24] = 2; // None for Option<TimelineId>.
    string(turn + 72, null);
    string(turn + 84, null);
    copyHex(turn + 48, uuid);
    u64(turn + 64, epoch);
    check(`StartTurn epoch ${epoch}`, encode('start_turn_encode', turn),
      `97c410${uuid}${encoded}90909090c2`);
  }
  const turn = alloc(104);
  bytes()[turn + 24] = 2;
  copyHex(turn + 48, uuid);
  u64(turn + 64, 42n);
  string(turn + 72, 'org');
  u32(turn, 1); // Some(BTreeNodeId(256)).
  u64(turn + 8, 256n);
  string(turn + 84, 'turn');
  bytes()[turn + 96] = 1;
  check('StartTurn present options', encode('start_turn_encode', turn),
    `97c410${uuid}2a91a36f726791cd010091a47475726e90c3`);

  for (const [name, encoded] of [[null, '90'], [runnerName, `91${runnerNameHex}`]]) {
    const request = alloc(304);
    string(request, name);
    check('SetLocalMachineRunner payload', encode('set_local_encode', request),
      `91${encoded}`);
    u32(request + 256, 39); // Rust enum discriminant, NOT wire tag 51.
    u32(request + 288, 42); // ClientMessage request correlation id.
    check('SetLocalMachineRunner full envelope', encode('client_encode', request),
      `922a923391${encoded}`);
  }
  const ack = alloc(40);
  u32(ack, 0xFFFFFFFF);
  copyHex(ack + 20, uuid);
  check('AcknowledgeLocalMachineTurn success', encode('ack_local_encode', ack),
    `92c410${uuid}90`);

  for (const [length, encoded] of integers) {
    const header = alloc(32);
    u64(header, length);
    copyHex(header + 8, oid);
    const expected = `93c0c414${oid}${encoded}`;
    check(`GitBlobFrameHeader length ${length}`, encode('git_header_encode', header),
      expected);
    for (const value of [expected, `94${expected.slice(2)}81a6667574757265c3`]) {
      const result = decode('git_header_decode', value, 64);
      check('header decode succeeds', view().getUint32(result.output, true), 0);
      check('header decode consumes input', result.remaining, 0);
      check('header decode length', view().getBigUint64(result.output + 8, true), length);
      check('header decode oid', readHex(result.output + 16, 20), oid);
    }
    for (const value of [expected.slice(0, -2), `92c0c414${oid}`,
      `93c0c413${oid.slice(0, -2)}${encoded}`]) {
      const result = decode('git_header_decode', value, 64);
      check('malformed header rejected', view().getUint32(result.output, true), 1);
    }
  }

  // ServerMessage::LocalMachineTurn: wire tag 18, Rust discriminant 14.
  const requestId = '101112131415161718191a1b1c1d1e1f';
  const historyId = '202122232425262728292a2b2c2d2e2f';
  const senderId = '303132333435363738393a3b3c3d3e3f';
  const startTurn = `97c410${senderId}2a90909090c2`;
  const fields = `c410${uuid}c410${requestId}c410${historyId}${startTurn}`;
  for (const hex of [`921294${fields}`, `921295${fields}81a6667574757265c3`]) {
    const result = decode('server_decode', hex, 512);
    check('LocalMachineTurn variant', bytes()[result.output], 14);
    check('LocalMachineTurn consumes input', result.remaining, 0);
    check('LocalMachineTurn machine', readHex(result.output + 1, 16), uuid);
    check('LocalMachineTurn request', readHex(result.output + 17, 16), requestId);
    check('LocalMachineTurn history', readHex(result.output + 33, 16), historyId);
    check('LocalMachineTurn sender', readHex(result.output + 104, 16), senderId);
    check('LocalMachineTurn epoch', view().getBigUint64(result.output + 120, true), 42n);
  }
  for (const hex of [`921294${fields.slice(0, -2)}`,
    `921293c410${uuid}c410${requestId}c410${historyId}`]) {
    const result = decode('server_decode', hex, 512);
    check('incomplete LocalMachineTurn rejected', bytes()[result.output], 255);
  }

  const dispatch = start =>
    `921294c410${uuid}c410${requestId}c410${historyId}${start}`;
  const startFields = [`c410${senderId}`, '2a', '90', '90', '90', '90', 'c2'];
  for (let length = 2; length < 7; length++) {
    const result = decode('server_decode',
      dispatch(`${(0x90 + length).toString(16)}${startFields.slice(0, length).join('')}`), 512);
    check('omitted StartTurn fields use defaults', bytes()[result.output], 14);
    check('omitted fields consume input', result.remaining, 0);
    check('default runner-started flag', bytes()[result.output + 152], 0);
  }
  for (let index = 2; index < 7; index++) {
    const values = startFields.map((value, i) => i === index ? 'c0' : value);
    const result = decode('server_decode', dispatch(`97${values.join('')}`), 512);
    check('nil StartTurn default accepted', bytes()[result.output], 14);
    check('nil default consumes input', result.remaining, 0);
  }
  for (const [tag, actor, binary] of [[0, oid, 'c414'], [1, uuid, 'c410']]) {
    for (const [timestamp, encoded] of integers) {
      const value = alloc(104);
      copyHex(value + 48, senderId);
      u64(value + 64, 18446744073709551615n);
      string(value + 72, 'org');
      u32(value, 1);
      u64(value + 8, 18446744073709551615n);
      string(value + 84, 'turn');
      u64(value + 16, timestamp);
      bytes()[value + 24] = tag;
      copyHex(value + 25, actor);
      bytes()[value + 96] = 1;
      const expected = `97c410${senderId}cfffffffffffffffff91a36f7267` +
        `91cfffffffffffffffff91a47475726e9192${encoded}92${tag.toString(16).padStart(2, '0')}${binary}${actor}c3`;
      check(`timeline actor ${tag}, timestamp ${timestamp}`,
        encode('start_turn_encode', value), expected);
      const result = decode('server_decode', dispatch(expected), 512);
      check('present StartTurn fields decode', bytes()[result.output], 14);
      check('present StartTurn consumes input', result.remaining, 0);
      check('decoded timeline timestamp', view().getBigUint64(result.output + 72, true), timestamp);
      check('decoded timeline actor tag', bytes()[result.output + 80], tag);
      check('decoded timeline actor ID', readHex(result.output + 81, actor.length / 2), actor);
      check('decoded runner-started flag', bytes()[result.output + 152], 1);
    }
  }
  for (const [index, invalid] of [
    [0, 'c0'], [0, `b0${senderId}`], [0, `dc0010${'01'.repeat(16)}`],
    [1, 'c0'], [1, 'ff'], [1, 'd0ff'], [1, 'ca42280000'],
    [2, '91c4036f7267'], [2, '92a36f726701'],
    [3, '91ff'], [3, '91c0'], [4, '91c4047475726e'],
    [5, '91c0'], [5, `9192ff9201c410${uuid}`],
    [5, `9192019202c410${uuid}`], [5, '91920101'],
    [5, `9192019200c410${uuid}`], [5, `9192019201c414${oid}`], [6, '00'],
  ]) {
    const values = startFields.map((value, i) => i === index ? invalid : value);
    const result = decode('server_decode', dispatch(`97${values.join('')}`), 512);
    check('malformed typed StartTurn field rejected', bytes()[result.output], 255);
  }
  const schemas = recoverSchemas(e);
  assert.deepEqual(recoverSchemas(e), schemas, 'schema descriptors must not contain uninitialized memory');

  // Independently compose the default root from the declared field paths.
  // A stub contains object references, not register values or map contents.
  const array = values => (values.length < 16
    ? (0x90 + values.length).toString(16)
    : `dc${values.length.toString(16).padStart(4, '0')}`) + values.join('');
  const fieldReference = fieldPath => `9201c4${fieldPath.length.toString(16).padStart(2, '0')}` +
    Buffer.from(fieldPath).toString('hex');
  const defaultStub = (name, fieldPath) => {
    if (name === 'deltadb::history::BufferId') {
      return `9200${fieldReference(fieldPath)}`; // Scratch buffer.
    }
    if (name.startsWith('deltadb::client::RegisterStub<') ||
        name.startsWith('deltadb::client::map::MapStub<')) {
      return fieldReference(fieldPath);
    }
    const definition = schemas.definitions[name];
    assert.equal(definition.kind, 3, `unhandled default stub ${name}`);
    const values = Array(Math.max(...Object.keys(definition.fields).map(Number)) + 1).fill('c0');
    for (const [index, field] of Object.entries(definition.fields)) {
      const schema = field.schema.kind === 23 ? field.schema.element : field.schema;
      assert.equal(schema.kind, 19, `expected named stub field ${name}/${index}`);
      values[Number(index)] = defaultStub(schema.named, [...fieldPath, Number(index)]);
    }
    return array(values);
  };
  for (const fieldPath of [[], [7, 9]]) {
    const expected = defaultStub('thread::ThreadDataStub', fieldPath);
    for (const input of ['90', '91c0', `dc0054${'c0'.repeat(84)}`, expected]) {
      const result = decode('thread_decode', input, 4096, fieldPath);
      check('default thread stub decoded', bytes()[result.output + 8] === 255, false);
      check('thread stub consumes input', result.remaining, 0);
      check('canonical default thread references', encode('thread_encode', result.output), expected);
    }
  }
  const invalidRoot = decode('thread_decode', 'c0', 4096, []);
  check('nil thread root rejected', bytes()[invalidRoot.output + 8], 255);
  let fieldId = alloc(16);
  const fieldPath = [7, 9, 23, 0, 255];
  for (let i = 0; i < fieldPath.length; i++) {
    const next = alloc(16);
    e.field_push(next, fieldId, fieldPath[i]);
    check('FieldId depth', bytes()[next], i + 1);
    check('FieldId path', readHex(next + 1, i + 1), Buffer.from(fieldPath.slice(0, i + 1)).toString('hex'));
    fieldId = next;
  }
  const anchors = anchorReference(e, {alloc, bytes, view, u32, decode, check});
  const textCases = verifyTextReference(e, {alloc, bytes, view, u32, u64, encode, decode, check, anchors});
  verifyValueReference(e, {alloc, bytes, view, u64, decode, check});
  verifyHistoryWriteReference(e, {alloc, bytes, u32, copyHex, encode, decode, check});
  assert.deepEqual(hostCalls, []);
  console.log(`PASS: ${checks} protocol/text checks; 0 browser/network/storage calls`);
  console.log(`Recovered ${Object.keys(schemas.definitions).length} thread/history schema descriptors`);
  console.log(`Generated ${textCases.length} native text-CRDT compatibility cases`);
  if (outputs['--schema-output']) {
    fs.writeFileSync(outputs['--schema-output'], JSON.stringify({
      version: metadata.version, build: metadata.build, ...schemas,
    }) + '\n', {flag: 'wx'});
  }
  if (outputs['--text-output']) {
    fs.writeFileSync(outputs['--text-output'], JSON.stringify({
      version: metadata.version, build: metadata.build, cases: textCases,
    }) + '\n', {flag: 'wx'});
  }
  console.log(`Reference: Delta ${metadata.version}, build ${metadata.build}`);
  console.log('Not tested: account authentication, live registration, history sync, or turns');
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
