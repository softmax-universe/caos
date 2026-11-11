#!/usr/bin/env node
// Offline compatibility probe, not a runner. No Delta account or application.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const {recoverSchemas} = require('./schema_reference.cjs');

const directory = process.argv[2];
const schemaOutput = process.argv[3] === '--schema-output' ? process.argv[4] : null;
if (!directory || !(process.argv.length === 3 || (process.argv.length === 5 && schemaOutput))) {
  console.error('usage: node verify_reference.cjs <prepared-reference-directory> [--schema-output <file>]');
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
      'server_decode', 'schema_thread', 'schema_batch'].sort(),
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
  const decode = (name, hex, size) => {
    const data = Buffer.from(hex, 'hex');
    const input = alloc(data.length);
    bytes().set(data, input);
    const slice = alloc(8), output = alloc(size), fieldId = alloc(24);
    u32(slice, input);
    u32(slice + 4, data.length);
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
  assert.deepEqual(hostCalls, []);
  console.log(`PASS: ${checks} codec checks; 0 browser/network/storage calls`);
  console.log(`Recovered ${Object.keys(schemas.definitions).length} thread/history schema descriptors`);
  if (schemaOutput) {
    fs.writeFileSync(schemaOutput, JSON.stringify({
      version: metadata.version, build: metadata.build, ...schemas,
    }) + '\n', {flag: 'wx'});
  }
  console.log(`Reference: Delta ${metadata.version}, build ${metadata.build}`);
  console.log('Not tested: account authentication, live registration, history sync, or turns');
}

main().catch(error => {
  console.error(error);
  process.exitCode = 1;
});
